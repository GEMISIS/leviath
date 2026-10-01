//! The embedded world's [`RunStarter`]: resolves and binds each run against
//! an [`EmbedEnv`] built from the values the embedder gave the builder.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{BasicToolService, EmbedEnv};
use crate::host::{PreparedRun, RunStarter};
use crate::pipeline::ModelDefaults;
use crate::provider_creds::ProviderCreds;
use crate::providers::ProviderRegistry;
use crate::resolve::{ResolveMode, resolve};
use crate::spec::env::{Caller, LoadedBlueprint, ResolveEnv as _};
use crate::spec::issues::SpawnIssues;
use crate::spec::request::{SpawnRequest, SpawnSource};
use crate::spec::summary::SpawnSummary;

/// How long a spawn waits for a gateway's model list it has not read yet.
/// Bounded so an unreachable gateway costs a spawn this and no more.
const PRIME_TIMEOUT_SECS: u64 = 10;

/// Blueprints handed to [`AgentWorld::spawn`](super::AgentWorld::spawn),
/// loaded, parked under the name the spawn's request carries until the
/// starter picks them up.
pub(crate) type StagedBlueprints = Arc<Mutex<HashMap<String, LoadedBlueprint>>>;

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
    /// Blueprints waiting for their spawn.
    pub staged: StagedBlueprints,
    /// The providers a bare model name may route to: the only ones whose
    /// unread model list can hold a stage back.
    pub preferred: Vec<String>,
}

impl EmbedStarter {
    /// The env a request is resolved and bound against: the world's
    /// providers and model defaults, with the request's staged blueprint
    /// registered under its own name, which the request is rewritten to name.
    fn env_for(&self, request: &mut SpawnRequest) -> EmbedEnv {
        let env = EmbedEnv::new(self.registry.clone(), self.defaults.clone());
        let mut limits = env.limits();
        limits.defaults.batch_tool_hint = self.hints.batch_tool;
        limits.defaults.shell_hint = self.hints.shell;
        let mut env = env.with_creds(self.creds.clone()).with_limits(limits);
        if let Some(tools) = &self.basic_tools {
            env = env.with_basic_tools(tools.clone());
        }
        if let SpawnSource::Blueprint(reference) = &mut request.source {
            let staged = leviath_core::sync::lock(&self.staged).remove(reference.name.as_str());
            if let Some(loaded) = staged {
                *reference = loaded.reference.clone();
                env = env.with_blueprint(loaded);
            }
        }
        env
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
        mut request: SpawnRequest,
        caller: Caller,
    ) -> Result<PreparedRun, SpawnIssues> {
        self.prime().await;
        let env = self.env_for(&mut request);
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
        mut request: SpawnRequest,
        caller: Caller,
    ) -> Result<SpawnSummary, SpawnIssues> {
        self.prime().await;
        let env = self.env_for(&mut request);
        let resolved = resolve(&request, &caller, &env, ResolveMode::Check).await?;
        Ok(SpawnSummary::of(&resolved.spec))
    }
}

/// Mint a run id: `<stem>-<unix-secs>-<counter>`. The per-process counter
/// keeps ids unique even when several spawns land in the same second.
pub(crate) fn mint_run_id(stem: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem: String = stem
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        })
        .collect();
    let stem = match stem.is_empty() {
        true => "agent".to_string(),
        false => stem,
    };
    format!("{stem}-{secs}-{n:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_run_ids_are_sanitized_and_unique() {
        let a = mint_run_id("My Coder!");
        let b = mint_run_id("My Coder!");
        assert!(a.starts_with("my-coder-"));
        assert_ne!(a, b);
        assert!(mint_run_id("").starts_with("agent-"));
    }
}
