//! Rebuilding the daemon's provider registry when `config.toml` changes.
//!
//! The registry is built from the config's keys, base URLs and
//! `[model_providers]` entries, and everything a person does to change where
//! their runs go - `lev setup`, `PUT /api/config`, editing the file by hand -
//! writes that file and nothing else. So the registry follows the file: move
//! off a provider whose credits have run out and the next run routes somewhere
//! else, with no daemon restart. The config reloader beside this module covers
//! the spawn-time settings out of the same file; this is the provider half.
//!
//! The check is on the credentials themselves rather than the file's mtime: a
//! save that changed a tool permission is not a reason to rebuild providers,
//! and a rebuild that dropped a provider mid-flight would be worse than the
//! staleness it cures. The comparison also names *which* providers changed, so
//! the circuit breaker can forget the failures that belong to a key the user
//! has since replaced.

use std::sync::{Arc, Mutex, PoisonError};

use leviath_runtime::{ProviderCreds, ProviderRegistry};

use crate::config::Config;

/// How long the rebuilt registry gets to read the model lists of the providers
/// that changed. The same budget the daemon's start-up prime uses, for the
/// same reason: the answer is an optimisation over the table compiled into
/// this build, so a provider that cannot answer in this long is better skipped
/// than allowed to hold up a run.
const PRIME_TIMEOUT_SECS: u64 = 10;

/// What a rebuild produced: the registry to install, and the providers whose
/// credentials are not the ones the old registry was built with.
struct Pending {
    registry: ProviderRegistry,
    changed: Vec<String>,
}

struct State {
    /// The credentials the current registry was built from, for comparison
    /// with the ones the config holds now.
    creds: Vec<ProviderCreds>,
    /// The newest registry, installed into the world or about to be.
    registry: ProviderRegistry,
    /// Built but not yet installed into the world. Taken by [`ProviderReload::install`].
    pending: Option<Pending>,
}

/// Keeps the daemon's provider registry in step with `config.toml`.
///
/// [`refresh`](Self::refresh) is the cheap synchronous check (rebuild only
/// when the credentials actually differ) and [`install`](Self::install) puts
/// the result into the world. They are separate because the world is only
/// reachable from the host's sync hooks, while the priming that makes a new
/// provider's model list known has to be awaited.
pub struct ProviderReload {
    state: Mutex<State>,
    /// How an HTTP-backed provider's client is built, injected so a test can
    /// build a registry without opening sockets.
    build_client: leviath_providers::provider::HttpClientFactory<'static>,
}

impl ProviderReload {
    /// Start from the registry the daemon booted with and the config it was
    /// built from.
    pub fn new(
        config: &Config,
        registry: ProviderRegistry,
        build_client: leviath_providers::provider::HttpClientFactory<'static>,
    ) -> Self {
        Self {
            state: Mutex::new(State {
                creds: crate::commands::run::session::provider_creds_from_config(config),
                registry,
                pending: None,
            }),
            build_client,
        }
    }

    /// The newest registry, for a caller that needs to ask a provider
    /// something before the world has it (the spawn preprocessor's model
    /// warming).
    pub fn registry(&self) -> ProviderRegistry {
        self.lock().registry.clone()
    }

    /// Rebuild from `config` when its provider credentials differ from the
    /// ones the current registry was built with, and hold the result for
    /// [`install`](Self::install). Returns the names whose credentials
    /// changed, empty when nothing did.
    ///
    /// A rebuild that fails keeps the current registry: an unusable provider
    /// entry should cost the run its new setting, not its providers.
    pub fn refresh(&self, config: &Config) -> Vec<String> {
        let creds = crate::commands::run::session::provider_creds_from_config(config);
        let mut state = self.lock();
        let changed = changed_names(&state.creds, &creds);
        if changed.is_empty() {
            return Vec::new();
        }
        // The script layer is carried over rather than rebuilt: it follows the
        // config through its own source, and holds the `.rhai` providers it has
        // already compiled.
        let built = leviath_runtime::provider_creds::build_provider_registry_with(
            &creds,
            self.build_client,
        );
        let registry = match built {
            Ok(registry) => match state.registry.script_layer() {
                Some(layer) => registry.with_script_layer(layer),
                None => registry,
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "config.toml changed its providers but the new set could not be built; \
                     keeping the ones already in service"
                );
                return Vec::new();
            }
        };
        state.creds = creds;
        state.registry = registry.clone();
        state.pending = Some(Pending {
            registry,
            changed: changed.clone(),
        });
        changed
    }

    /// [`refresh`](Self::refresh), then read the model lists of the providers
    /// it built, so a run resolving against the new set is sized against what
    /// those providers actually serve rather than the table compiled into this
    /// build.
    pub async fn refresh_and_prime(&self, config: &Config) -> Vec<String> {
        let changed = self.refresh(config);
        if !changed.is_empty() {
            let registry = self.registry();
            let default = config.default_provider.clone();
            registry
                .prime_capabilities(
                    std::time::Duration::from_secs(PRIME_TIMEOUT_SECS),
                    &[default.as_str()],
                )
                .await;
        }
        changed
    }

    /// Install whatever [`refresh`](Self::refresh) built into `world`. Does
    /// nothing when there is nothing pending, so it is safe to call before
    /// every spawn.
    pub fn install(&self, world: &mut leviath_runtime::PipelineWorld) {
        let pending = self.lock().pending.take();
        if let Some(Pending { registry, changed }) = pending {
            world.replace_providers(registry, &changed);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The provider names whose credentials `new` states differently from `old`:
/// added, removed, or the same name with a different key, URL, timeout or
/// rate limit. Sorted, so the log line and the tests read the same way twice.
fn changed_names(old: &[ProviderCreds], new: &[ProviderCreds]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for cred in new {
        match old.iter().find(|c| c.name == cred.name) {
            Some(before) if before == cred => {}
            _ => names.push(cred.name.clone()),
        }
    }
    for cred in old {
        if !new.iter().any(|c| c.name == cred.name) {
            names.push(cred.name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Build one for a caller that has a config and wants the daemon's registry
/// semantics, with the real HTTP client factory.
pub fn for_daemon(config: &Config, registry: ProviderRegistry) -> Arc<ProviderReload> {
    Arc::new(ProviderReload::new(
        config,
        registry,
        &leviath_providers::provider::build_http_client,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_factory() -> leviath_providers::provider::HttpClientFactory<'static> {
        &leviath_providers::provider::build_http_client
    }

    fn config_with_key(key: &str) -> Config {
        Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some(key.to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        }
    }

    fn boot(config: &Config) -> ProviderReload {
        ProviderReload::new(config, boot_registry(config), client_factory())
    }

    /// The registry a daemon boots with, without the Ollama reachability probe
    /// opening a socket.
    fn boot_registry(config: &Config) -> ProviderRegistry {
        leviath_runtime::provider_creds::build_provider_registry_probing(
            &crate::commands::run::session::provider_creds_from_config(config),
            client_factory(),
            &|_| false,
        )
        .unwrap()
    }

    #[test]
    fn an_unchanged_config_rebuilds_nothing() {
        let config = config_with_key("sk-ant");
        let reload = boot(&config);
        assert!(reload.refresh(&config).is_empty());
        assert!(reload.lock().pending.is_none());
    }

    #[test]
    fn a_new_provider_key_is_a_change_and_the_registry_gains_it() {
        let config = config_with_key("sk-ant");
        let reload = boot(&config);
        assert!(!reload.registry().has("openrouter"));

        let mut after = config.clone();
        after.openrouter_api_key = Some("sk-or".to_string());
        assert_eq!(reload.refresh(&after), vec!["openrouter".to_string()]);
        assert!(
            reload.registry().has("openrouter"),
            "the provider the user just configured has to be usable without a restart"
        );
    }

    #[test]
    fn a_removed_key_drops_the_provider() {
        let config = config_with_key("sk-ant");
        let reload = boot(&config);
        assert!(reload.registry().has("anthropic"));

        let mut after = config.clone();
        after.providers.anthropic_api_key = None;
        assert_eq!(reload.refresh(&after), vec!["anthropic".to_string()]);
        assert!(
            !reload.registry().has("anthropic"),
            "a provider the user untoggled must stop being a route"
        );
    }

    #[test]
    fn replacing_a_key_reports_that_provider_alone() {
        let mut config = config_with_key("sk-ant-old");
        config.openrouter_api_key = Some("sk-or".to_string());
        let reload = boot(&config);

        let mut after = config.clone();
        after.providers.anthropic_api_key = Some("sk-ant-new".to_string());
        assert_eq!(reload.refresh(&after), vec!["anthropic".to_string()]);
    }

    #[test]
    fn a_change_elsewhere_in_the_config_is_not_a_provider_change() {
        let config = config_with_key("sk-ant");
        let reload = boot(&config);

        let mut after = config.clone();
        after.limits.max_concurrent_tools = 99;
        assert!(
            reload.refresh(&after).is_empty(),
            "a limits edit must not rebuild the providers under a running daemon"
        );
    }

    #[test]
    fn an_endpoint_entrys_base_url_change_is_a_change() {
        let mut config = Config::default();
        config.model_providers.insert(
            "gateway".to_string(),
            crate::config::ModelProviderConfig {
                kind: Some(crate::config::ModelProviderKind::OpenaiCompatible),
                base_url: Some("http://127.0.0.1:9001/v1".to_string()),
                ..Default::default()
            },
        );
        let reload = boot(&config);

        let mut after = config.clone();
        after
            .model_providers
            .get_mut("gateway")
            .unwrap()
            .base_url
            .replace("http://127.0.0.1:9002/v1".to_string());
        assert_eq!(reload.refresh(&after), vec!["gateway".to_string()]);
    }

    #[test]
    fn install_hands_the_pending_registry_to_the_world_once() {
        let config = config_with_key("sk-ant");
        let reload = boot(&config);
        let mut after = config.clone();
        after.openrouter_api_key = Some("sk-or".to_string());
        reload.refresh(&after);

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let mut world = leviath_runtime::PipelineWorld::new(
            leviath_runtime::ProviderRegistry::new(),
            std::sync::Arc::new(crate::daemon::tool_service::CliToolService::new()),
            leviath_runtime::inference_pool::InferencePoolConfig::new(),
            1,
            None,
            runtime.handle().clone(),
        );
        reload.install(&mut world);
        assert!(world.providers().has("openrouter"));
        assert!(
            reload.lock().pending.is_none(),
            "the pending rebuild is consumed, not re-applied on every spawn"
        );
    }

    #[tokio::test]
    async fn refresh_and_prime_reports_the_same_names_as_refresh() {
        let config = config_with_key("sk-ant");
        let reload = boot(&config);
        let mut after = config.clone();
        after.providers.anthropic_api_key = None;
        assert_eq!(
            reload.refresh_and_prime(&after).await,
            vec!["anthropic".to_string()]
        );
        // Nothing changed the second time, so nothing is primed either.
        assert!(reload.refresh_and_prime(&after).await.is_empty());
    }

    #[test]
    fn a_rebuild_keeps_the_script_provider_layer() {
        // A `.rhai` provider the daemon has already compiled must survive a
        // key change elsewhere: rebuilding the layer would drop it, and the
        // layer follows the config on its own anyway.
        let config = config_with_key("sk-ant");
        let dir = tempfile::tempdir().unwrap();
        let layer =
            std::sync::Arc::new(leviath_runtime::script_provider::ScriptProviderLayer::new(
                dir.path().to_path_buf(),
                Default::default(),
                Default::default(),
                None,
                Vec::new(),
            ));
        let reload = ProviderReload::new(
            &config,
            boot_registry(&config).with_script_layer(layer),
            client_factory(),
        );

        let mut after = config.clone();
        after.openrouter_api_key = Some("sk-or".to_string());
        reload.refresh(&after);
        assert!(reload.registry().script_layer().is_some());
    }

    #[test]
    fn a_registry_that_cannot_be_built_leaves_the_working_one_in_service() {
        // The one failure this has: no usable HTTPS client. Losing the
        // providers already in service over it would be worse than ignoring
        // the edit, so the edit is what is dropped.
        let config = config_with_key("sk-ant");
        let reload = ProviderReload::new(&config, boot_registry(&config), &|_t| {
            Err(leviath_providers::provider::malformed_url_error())
        });

        let mut after = config.clone();
        after.openrouter_api_key = Some("sk-or".to_string());
        assert!(reload.refresh(&after).is_empty());
        assert!(
            reload.registry().has("anthropic"),
            "the registry the daemon has been running on is still there"
        );
    }

    #[test]
    fn for_daemon_starts_from_the_registry_it_is_given() {
        let config = config_with_key("sk-ant");
        let registry = leviath_runtime::provider_creds::build_provider_registry_probing(
            &crate::commands::run::session::provider_creds_from_config(&config),
            client_factory(),
            &|_| false,
        )
        .unwrap();
        let reload = for_daemon(&config, registry);
        assert!(reload.registry().has("anthropic"));
        assert!(
            reload.refresh(&config).is_empty(),
            "the config it was built from is not a change"
        );
    }
}
