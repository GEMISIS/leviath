//! One-shot run-title generation.
//!
//! The dashboard displays, searches, and persists `RunMetadata.title`; this
//! module is what fills it in. At spawn, the daemon marks an eligible run
//! [`PendingTitle`]; [`dispatch_title`] makes one cheap LLM call over the
//! task prompt via the `title_bridge` worker, and [`collect_title`]
//! sanitizes the reply into the metadata. Everything downstream (persistence,
//! dashboard header, run search) already reads the field.
//!
//! Best-effort by design: any failure - no usable provider or model, a full
//! pool that never frees, a provider error, an empty reply - leaves the title
//! `None` and the run displays its blueprint name exactly as before. The
//! title lands on disk with the next persistence write (the run-level
//! heartbeat guarantees one within a few seconds).

use bevy_ecs::prelude::{Commands, Component, Entity, Query, Res, ResMut, Resource, With, Without};
use leviath_providers::InferenceRequest;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::persistence::RunMetadata;
use crate::pipeline::{InferenceStage, Providers};
use crate::title_bridge::{TitleJob, TitleOutcome, run_title_job};

/// The `[title]` config, as a world resource (inserted by the daemon at
/// setup). Absent in worlds that never title (tests, `lev run` without it).
#[derive(Resource, Clone)]
pub struct TitleSettings(pub leviath_core::config::TitleConfig);

/// This run wants a title; `dispatch_title` picks it up on the next tick.
/// Inserted at spawn for enabled, root, non-empty-task runs only.
#[derive(Component, Debug, Clone, Copy)]
pub struct PendingTitle;

/// A title call is in flight. Unlike `AwaitingCompaction`, this does not hold
/// the agent out of inference - titling runs alongside the first turn.
#[derive(Component, Debug, Clone, Copy)]
pub struct AwaitingTitle;

/// The receiving end of the title-outcomes channel, as a world resource.
#[derive(Resource)]
pub struct TitleResults(pub UnboundedReceiver<TitleOutcome>);

/// The sending end, cloned into each spawned title job.
#[derive(Resource)]
pub struct TitleSink(pub UnboundedSender<TitleOutcome>);

/// Kept small: a title is one short line, and a runaway reply is cut anyway.
const TITLE_MAX_TOKENS: usize = 64;
/// How much of the task prompt the model sees. Titles come from the opening
/// framing of a task, not its appendix.
const TITLE_TASK_BUDGET: usize = 2_000;
/// Display cap, in bytes, cut on a char boundary.
const TITLE_MAX_LEN: usize = 80;

const TITLE_SYSTEM_PROMPT: &str = "Reply with only a short title for the given task, \
     at most 8 words. No quotes, no trailing punctuation, no explanation.";

/// Resolve which provider/model the title call should use.
///
/// `[title]` config wins where set; unset fields fall back to the run's own
/// first-stage provider and model (from the `provider/model` label). The one
/// unguessable case - an explicit title provider that differs from the run's,
/// with no title model - resolves to `None`, because the run's model name
/// means nothing to another provider.
fn resolve_title_model(
    settings: &leviath_core::config::TitleConfig,
    run_model_label: Option<&str>,
) -> Option<(String, String)> {
    let run = run_model_label.and_then(|label| label.split_once('/'));
    let provider = settings
        .provider
        .clone()
        .or_else(|| run.map(|(p, _)| p.to_string()))?;
    let model = settings.model.clone().or_else(|| match run {
        Some((run_provider, run_model)) if run_provider == provider => Some(run_model.to_string()),
        _ => None,
    })?;
    Some((provider, model))
}

/// Build the one-shot titling request over the task prompt.
fn title_request(task: &str, model: &str) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![
            leviath_providers::Message {
                role: "system".to_string(),
                content: TITLE_SYSTEM_PROMPT.to_string().into(),
                cache_breakpoint: false,
            },
            leviath_providers::Message {
                role: "user".to_string(),
                content: leviath_core::truncate_at_boundary(task, TITLE_TASK_BUDGET)
                    .to_string()
                    .into(),
                cache_breakpoint: false,
            },
        ],
        model: model.to_string(),
        max_tokens: TITLE_MAX_TOKENS,
        temperature: 0.2,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

/// Reduce a raw model reply to a displayable one-line title: the first
/// non-empty line, unquoted, capped at [`TITLE_MAX_LEN`]. Empty means "no
/// title" and the metadata stays untouched.
fn sanitize_title(raw: &str) -> String {
    let first = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let unquoted = first.trim_matches(['"', '\'', '`']).trim();
    leviath_core::truncate_at_boundary(unquoted, TITLE_MAX_LEN)
        .trim_end()
        .to_string()
}

/// Dispatch system: start the title call for each [`PendingTitle`] run.
///
/// A full pool leaves the marker in place to retry next tick; every other
/// dead end (no settings resource, no resolvable provider/model, provider
/// not registered) drops the marker so the query empties instead of spinning.
#[allow(clippy::type_complexity)]
pub fn dispatch_title(
    agents: Query<(Entity, &RunMetadata), (With<PendingTitle>, Without<AwaitingTitle>)>,
    settings: Option<Res<TitleSettings>>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    sink: Res<TitleSink>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, meta) in agents.iter() {
        crate::tick_scope::enter(entity);
        let resolved = settings
            .as_ref()
            .filter(|s| s.0.enabled)
            .and_then(|s| resolve_title_model(&s.0, meta.model.as_deref()));
        let Some((provider_name, model)) = resolved else {
            tracing::debug!(run_id = %meta.run_id, "no usable title provider/model; skipping");
            commands.entity(entity).remove::<PendingTitle>();
            continue;
        };
        let Some(provider) = providers.0.get(&provider_name) else {
            tracing::debug!(
                run_id = %meta.run_id,
                provider = %provider_name,
                "title provider not registered; skipping"
            );
            commands.entity(entity).remove::<PendingTitle>();
            continue;
        };
        let Some(permit) = stage.pools.try_acquire(&model) else {
            continue; // pool full - retry next tick
        };

        stage.runtime.spawn(run_title_job(
            TitleJob {
                entity,
                provider,
                request: title_request(&meta.task, &model),
                permit,
            },
            std::time::Duration::from_secs(leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS),
            sink.0.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<PendingTitle>()
            .insert(AwaitingTitle);
    }
}

/// Collect system: store each finished title into its run's metadata. A
/// provider error or empty reply changes nothing; either way the in-flight
/// marker comes off.
pub fn collect_title(
    mut results: ResMut<TitleResults>,
    mut agents: Query<&mut RunMetadata, With<AwaitingTitle>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok(mut meta) = agents.get_mut(outcome.entity) else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        if let Ok(raw) = outcome.result {
            let title = sanitize_title(&raw);
            if !title.is_empty() {
                meta.title = Some(title);
            }
        }
        commands.entity(outcome.entity).remove::<AwaitingTitle>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::schedule::Schedule;
    use bevy_ecs::world::World;
    use leviath_providers::{Provider, ProviderError};
    use std::sync::Arc;
    use tokio::runtime::Handle;
    use tokio::sync::Notify;
    use tokio::sync::mpsc;

    /// A provider whose single call yields a fixed reply or a fixed error.
    struct Scripted(Result<&'static str, &'static str>);

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn infer(
            &self,
            _r: &InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            match self.0 {
                Ok(reply) => Ok(leviath_providers::InferenceResponse {
                    content: reply.to_string(),
                    tool_calls: vec![],
                    tokens_used: leviath_providers::TokenUsage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    finish_reason: leviath_providers::FinishReason::Complete,
                }),
                Err(msg) => Err(ProviderError::Other(msg.to_string())),
            }
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn metadata(model: Option<&str>) -> RunMetadata {
        RunMetadata {
            run_id: "run-t".to_string(),
            agent_name: "titled".to_string(),
            agent_path: "/a".to_string(),
            task: "summarize the release notes".to_string(),
            model: model.map(str::to_string),
            workdir: "/w".to_string(),
            num_stages: 1,
            started_at: 0,
            parent_run_id: None,
            metadata: Default::default(),
            callback_url: None,
            callback_secret: None,
            title: None,
            unattended: false,
            read_paths: None,
            output_request: None,
        }
    }

    /// A world with the title lane wired over the given provider outcome, plus
    /// the receiver the dispatched job reports into.
    fn build_world(
        reply: Result<&'static str, &'static str>,
        pools: crate::inference_pool::InferencePools,
    ) -> (World, mpsc::UnboundedReceiver<TitleOutcome>) {
        let mut registry = crate::ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(Scripted(reply)));
        let (title_tx, title_rx) = mpsc::unbounded_channel();
        let (inf_tx, _inf_rx) = mpsc::unbounded_channel();
        let (ttx, _trx) = mpsc::unbounded_channel();
        let (ctx, _crx) = mpsc::unbounded_channel();
        let (cstx, _csrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(Providers(registry));
        world.insert_resource(InferenceStage {
            pools: Arc::new(pools),
            outcomes: inf_tx,
            transition_outcomes: ttx,
            compaction_outcomes: ctx,
            content_summary_outcomes: cstx,
            wake: Arc::new(Notify::new()),
            runtime: Handle::current(),
            exact_token_counting: false,
        });
        world.insert_resource(TitleSink(title_tx));
        (world, title_rx)
    }

    fn run_dispatch(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_title);
        schedule.run(world);
    }

    fn run_collect(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(collect_title);
        schedule.run(world);
    }

    fn default_pools() -> crate::inference_pool::InferencePools {
        crate::inference_pool::InferencePools::new(crate::inference_pool::InferencePoolConfig::new())
    }

    /// The permit must come back within the deadline even when the provider
    /// never answers - a hung title call once held its pool slot forever.
    #[tokio::test]
    async fn title_deadline_frees_the_slot_when_the_provider_hangs() {
        struct Hang;
        #[async_trait::async_trait]
        impl Provider for Hang {
            async fn infer(
                &self,
                _r: &InferenceRequest,
            ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
                std::future::pending().await
            }
            async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
                1
            }
            fn max_context_tokens(&self, _m: &str) -> usize {
                100_000
            }
            fn name(&self) -> &str {
                "hang"
            }
            fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
                leviath_providers::ModelCapabilities::default()
            }
        }

        // Trait obligations on the mock; only `infer` matters to this test.
        assert_eq!(Hang.count_tokens("t", "m").await, 1);
        assert_eq!(Hang.max_context_tokens("m"), 100_000);
        assert_eq!(Hang.name(), "hang");
        let _ = Hang.capabilities("m");
        let pools = crate::inference_pool::InferencePools::new(
            crate::inference_pool::InferencePoolConfig::new(),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_title_job(
            TitleJob {
                entity: bevy_ecs::entity::Entity::PLACEHOLDER,
                provider: Arc::new(Hang),
                request: title_request("task", "m"),
                permit: pools.try_acquire("m").expect("free"),
            },
            std::time::Duration::from_millis(5),
            tx,
            Arc::new(Notify::new()),
        )
        .await;
        let outcome = rx.recv().await.expect("an outcome is always reported");
        let err = outcome.result.expect_err("the deadline must surface");
        assert!(err.to_string().contains("deadline"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_and_collect_set_the_title() {
        let (mut world, title_rx) = build_world(Ok("\"Release notes digest\"\n"), default_pools());
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();

        run_dispatch(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert!(world.get::<AwaitingTitle>(e).is_some());

        // Await the spawned job's report, then hand it to the collector
        // through the results resource.
        let mut title_rx = title_rx;
        let outcome = title_rx.recv().await.expect("job reported");
        assert_eq!(outcome.entity, e);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));

        run_collect(&mut world);
        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title.as_deref(),
            Some("Release notes digest")
        );
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    #[tokio::test]
    async fn provider_error_leaves_the_title_unset() {
        let (mut world, mut title_rx) = build_world(Err("boom"), default_pools());
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();

        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        assert!(outcome.result.is_err());
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));

        run_collect(&mut world);
        assert_eq!(world.get::<RunMetadata>(e).unwrap().title, None);
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    #[tokio::test]
    async fn whitespace_reply_leaves_the_title_unset() {
        let (mut world, mut title_rx) = build_world(Ok("  \n \n"), default_pools());
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();

        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));

        run_collect(&mut world);
        assert_eq!(world.get::<RunMetadata>(e).unwrap().title, None);
    }

    #[tokio::test]
    async fn collect_skips_a_despawned_agent() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        let (tx, rx) = mpsc::unbounded_channel();
        // A ghost entity id: spawn then despawn.
        let ghost = world.spawn(metadata(Some("mock/m"))).id();
        world.despawn(ghost);
        tx.send(TitleOutcome {
            entity: ghost,
            result: Ok("t".to_string()),
        })
        .unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world); // must not panic
    }

    #[tokio::test]
    async fn dispatch_without_settings_drops_the_marker() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_with_disabled_settings_drops_the_marker() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        world.insert_resource(TitleSettings(leviath_core::config::TitleConfig {
            enabled: false,
            provider: None,
            model: None,
        }));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_with_unregistered_provider_drops_the_marker() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        world.insert_resource(TitleSettings(config(Some("nowhere"), Some("m"))));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_retries_while_the_pool_is_full() {
        let mut cfg = crate::inference_pool::InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = crate::inference_pool::InferencePools::new(cfg);
        let held = pools.try_acquire("m").unwrap();
        let (mut world, _title_rx) = build_world(Ok("t"), pools);
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();

        run_dispatch(&mut world);
        // Slot occupied: the marker stays so the next tick retries.
        assert!(world.get::<PendingTitle>(e).is_some());
        assert!(world.get::<AwaitingTitle>(e).is_none());
        drop(held);
    }

    fn config(provider: Option<&str>, model: Option<&str>) -> leviath_core::config::TitleConfig {
        leviath_core::config::TitleConfig {
            enabled: true,
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn resolve_prefers_the_configured_pair() {
        assert_eq!(
            resolve_title_model(
                &config(Some("openai"), Some("gpt-5-mini")),
                Some("anthropic/m")
            ),
            Some(("openai".to_string(), "gpt-5-mini".to_string()))
        );
    }

    #[test]
    fn resolve_falls_back_to_the_runs_provider_and_model() {
        assert_eq!(
            resolve_title_model(&config(None, None), Some("anthropic/claude-x")),
            Some(("anthropic".to_string(), "claude-x".to_string()))
        );
    }

    #[test]
    fn resolve_borrows_the_runs_model_only_for_the_same_provider() {
        assert_eq!(
            resolve_title_model(&config(Some("anthropic"), None), Some("anthropic/claude-x")),
            Some(("anthropic".to_string(), "claude-x".to_string()))
        );
        // A different provider cannot use the run's model name.
        assert_eq!(
            resolve_title_model(&config(Some("openai"), None), Some("anthropic/claude-x")),
            None
        );
    }

    #[test]
    fn resolve_gives_up_without_any_provider_or_model() {
        assert_eq!(resolve_title_model(&config(None, None), None), None);
        assert_eq!(resolve_title_model(&config(None, Some("m")), None), None);
        // A label without a slash carries no provider/model split.
        assert_eq!(
            resolve_title_model(&config(None, None), Some("bare-label")),
            None
        );
    }

    #[test]
    fn title_request_truncates_the_task_and_carries_the_model() {
        let long_task = "x".repeat(5_000);
        let req = title_request(&long_task, "gpt-5-mini");
        assert_eq!(req.model, "gpt-5-mini");
        assert_eq!(req.max_tokens, TITLE_MAX_TOKENS);
        assert_eq!(req.messages.len(), 2);
        let expected: leviath_providers::MessageContent =
            leviath_core::truncate_at_boundary(&long_task, TITLE_TASK_BUDGET)
                .to_string()
                .into();
        assert_eq!(req.messages[1].content, expected);
    }

    #[tokio::test]
    async fn scripted_provider_metadata_is_exercised() {
        // Keep the mock's non-`infer` trait methods measured.
        let p = Scripted(Ok("t"));
        assert_eq!(p.name(), "mock");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let default = leviath_providers::ModelCapabilities::default();
        assert_eq!(
            p.capabilities("m").max_output_tokens,
            default.max_output_tokens
        );
    }

    #[test]
    fn sanitize_takes_the_first_line_unquoted_and_capped() {
        assert_eq!(
            sanitize_title("\"Fix the login bug\"\nextra"),
            "Fix the login bug"
        );
        assert_eq!(
            sanitize_title("\n\n  'Tidy: workspace'  \n"),
            "Tidy: workspace"
        );
        assert_eq!(sanitize_title("   \n\t\n"), "");
        let long = "word ".repeat(40);
        assert!(sanitize_title(&long).len() <= TITLE_MAX_LEN);
    }
}
