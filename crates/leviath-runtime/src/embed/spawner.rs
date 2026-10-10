//! The embedded world's [`RunStarter`]: resolves and binds each run against
//! an [`EmbedEnv`] built from the values the embedder gave the builder.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{BasicToolService, EmbedEnv};
use crate::host::{PreparedRun, RunStarter};
use crate::pipeline::ModelDefaults;
use crate::provider_creds::ProviderCreds;
use crate::providers::ProviderRegistry;
use crate::resolve::{ResolveMode, resolve};
use crate::spec::env::{Caller, LoadedBlueprint, ResolveEnv as _};
use crate::spec::issues::SpawnIssues;
use crate::spec::request::SpawnRequest;
use crate::spec::summary::SpawnSummary;

/// How long a spawn waits for a gateway's model list it has not read yet.
/// Bounded so an unreachable gateway costs a spawn this and no more.
const PRIME_TIMEOUT_SECS: u64 = 10;

/// The blueprints a world's requests may name, by name. Shared between the
/// world, which adds to it, and its starter, which reads it at each spawn.
pub(crate) type Blueprints = Arc<Mutex<BTreeMap<String, LoadedBlueprint>>>;

/// Everything an embedded world starts its runs with.
pub(crate) struct EmbedStarter {
    /// The world's providers, shared with it.
    pub registry: ProviderRegistry,
    /// The credentials the registry was built from, for fingerprints.
    pub creds: Vec<ProviderCreds>,
    /// How models are chosen.
    pub defaults: ModelDefaults,
    /// The framework's prompt hints, as the builder set them.
    pub hints: leviath_core::config::PromptHints,
    /// The default tool service, which each bound run is registered with;
    /// `None` when the embedder installed its own.
    pub basic_tools: Option<Arc<BasicToolService>>,
    /// The blueprints requests may name.
    pub blueprints: Blueprints,
    /// The workdir of a request that names none.
    pub workdir: Option<PathBuf>,
    /// The providers a bare model name may route to: the only ones whose
    /// unread model list can hold a stage back.
    pub preferred: Vec<String>,
}

impl EmbedStarter {
    /// The env a request is resolved and bound against: the world's
    /// providers, model defaults and blueprints as they are now.
    fn env(&self) -> EmbedEnv {
        let env = EmbedEnv::new(self.registry.clone(), self.defaults.clone());
        let mut limits = env.limits();
        limits.defaults.batch_tool_hint = self.hints.batch_tool;
        limits.defaults.shell_hint = self.hints.shell;
        let mut env = env.with_creds(self.creds.clone()).with_limits(limits);
        if let Some(tools) = &self.basic_tools {
            env = env.with_basic_tools(tools.clone());
        }
        if let Some(dir) = &self.workdir {
            env = env.with_default_workdir(dir.clone());
        }
        let blueprints = leviath_core::sync::lock(&self.blueprints).clone();
        blueprints
            .into_values()
            .fold(env, |env, blueprint| env.with_blueprint(blueprint))
    }

    /// Ask any gateway whose model list is unread for it, so a stage that
    /// names a bare model resolves against the real list. A list already in
    /// hand costs nothing.
    async fn prime(&self) {
        let preferred: Vec<&str> = self.preferred.iter().map(String::as_str).collect();
        self.registry
            .prime_unread(
                std::time::Duration::from_secs(PRIME_TIMEOUT_SECS),
                &preferred,
            )
            .await;
    }
}

#[async_trait::async_trait]
impl RunStarter for EmbedStarter {
    async fn start(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<PreparedRun, SpawnIssues> {
        self.prime().await;
        let env = self.env();
        let resolved = resolve(&request, &caller, &env, ResolveMode::Spawn).await?;
        let bindings = crate::bind::bind(&resolved.spec, &resolved.code, &env).await?;
        let state = crate::insert::initial_state(&resolved.spec);
        Ok(PreparedRun {
            spec: Arc::new(resolved.spec),
            bindings,
            state,
        })
    }

    async fn check(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<SpawnSummary, SpawnIssues> {
        self.prime().await;
        let env = self.env();
        let resolved = resolve(&request, &caller, &env, ResolveMode::Check).await?;
        Ok(SpawnSummary::of(&resolved.spec))
    }
}
