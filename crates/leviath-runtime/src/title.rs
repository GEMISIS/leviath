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
//! pool that never frees, a provider error, an empty reply, a reply cut off at
//! the token limit, or one holding no line short enough to be a title - leaves
//! the title `None`, and the run shows the task the user typed instead. The
//! title lands on disk with the next persistence write (the run-level
//! heartbeat guarantees one within a few seconds).
//!
//! That last pair of refusals matters more than it sounds. A reasoning model
//! spends output tokens working up to its answer, so a starved budget returns
//! nothing but working-out - and the OpenAI-shaped parsers promote that into
//! `content` when the answer itself is empty. The title call asks the provider
//! not to think, gives it room in case it does anyway, and stores nothing at
//! all rather than the model's thoughts about what a title should be.

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

/// Enough for a model that thinks before it answers to get past the thinking.
///
/// This was 64 on the reasoning that a title is one short line. It is, but a
/// reasoning model spends output tokens working up to it, and 64 of them are
/// gone before it writes a word of title - which is how the *reasoning* came
/// to be stored as the title. The call happens once per run, so the extra
/// headroom costs nothing worth counting, and `sanitize_title` still refuses
/// anything longer than a title.
const TITLE_MAX_TOKENS: usize = 512;
/// How much of the task prompt the model sees. Titles come from the opening
/// framing of a task, not its appendix.
const TITLE_TASK_BUDGET: usize = 2_000;
/// Display cap, in bytes. A reply with no line this short has no title in it.
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

/// The provider's own "do not think about this one" switch.
///
/// A title does not need a reasoning pass, and a model that takes one spends
/// its output budget before it writes any title at all. Worse, a reasoning
/// model that answers with an empty `content` has its working-out promoted
/// into `content` by the OpenAI-shaped parsers, so what comes back is prose
/// about generating a title rather than a title.
///
/// Keyed on the resolved provider because each API spells the switch
/// differently and rejects the others' spelling outright. The providers not
/// listed need nothing: Anthropic only thinks when a request asks it to, and
/// OpenAI never returns reasoning text, so `reasoning_effort` would buy
/// nothing here while risking a 400 on a model with no reasoning to configure.
fn no_thinking_extra(provider: &str) -> serde_json::Value {
    match provider {
        // Ollama's own switch, which its provider lifts to the top level of
        // the body. Only sent when asked, because a model with no thinking to
        // turn off rejects the field.
        "ollama" => serde_json::json!({ "think": false }),
        // OpenRouter's unified reasoning control, merged into the body at the
        // top level. This is the provider that hands reasoning back as the
        // reply when the answer is empty, so it is the one that leaked.
        "openrouter" => serde_json::json!({ "reasoning": { "enabled": false } }),
        _ => serde_json::Value::Null,
    }
}

/// Build the one-shot titling request over the task prompt.
fn title_request(task: &str, provider: &str, model: &str) -> InferenceRequest {
    InferenceRequest {
        // A system *block*, not a message with `role: "system"`. That is the
        // portable shape: each provider maps blocks to whatever its API wants,
        // and Anthropic's Messages API - the default for every shipped
        // blueprint - rejects a `system` role inside `messages` outright with a
        // 400. Titling therefore failed for essentially every user, silently,
        // because a failed title is by design not worth interrupting a run for
        // and the reason only reached a debug log in a daemon whose output goes
        // nowhere.
        system: vec![leviath_providers::SystemBlock {
            text: TITLE_SYSTEM_PROMPT.to_string(),
            cache_hint: leviath_core::CacheHint::Never,
            volatility: leviath_core::Volatility::default(),
            region: String::new(),
        }],
        messages: vec![leviath_providers::Message {
            role: "user".to_string(),
            content: leviath_core::truncate_at_boundary(task, TITLE_TASK_BUDGET)
                .to_string()
                .into(),
            cache_breakpoint: false,
        }],
        model: model.to_string(),
        max_tokens: TITLE_MAX_TOKENS,
        temperature: 0.2,
        tools: Vec::new(),
        extra: no_thinking_extra(provider),
        request_timeout_secs: None,
    }
}

/// Reduce a raw model reply to a displayable one-line title: the first
/// non-empty line that is short enough to *be* a title, unquoted. Empty means
/// "this reply holds no title" and the metadata stays untouched.
fn sanitize_title(raw: &str) -> String {
    // Reasoning models answer the instruction *after* thinking about it out
    // loud, so the leading text is prose about the task rather than a title.
    // The rule that separates them without guessing at content: a title fits
    // the display cap, and reasoning does not.
    //
    // A reply with no line that fits has no title in it, so this returns
    // nothing and the run keeps showing its task text. It used to fall back to
    // the first line *truncated*, which is how a run came to be titled "We
    // need to generate a short title for the user's request. The user wants to
    // buil" - one unbroken paragraph of reasoning, cut at exactly the display
    // cap. Truncating prose does not make it a title; it only hides that this
    // failed.
    let stripped = strip_reasoning(raw);
    // Compared in bytes, which is what the cap is in. Counting chars here and
    // cutting bytes afterwards meant a title of 80 CJK characters passed the
    // check and was then sliced mid-title.
    stripped
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_matches(['"', '\'', '`']).trim().to_string())
        .find(|l| l.len() <= TITLE_MAX_LEN)
        .unwrap_or_default()
}

/// Opening tags a model may wrap its reasoning in, with the closer that ends
/// each. Ollama returns thinking in its own field and Anthropic in its own
/// block, but a local GGUF writes the tags inline in the reply text and no
/// provider strips them.
const REASONING_TAGS: [(&str, &str); 3] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];

/// Drop the reasoning blocks a model emits around its working-out. An unclosed
/// tag drops the rest, which is the safe reading: what follows an opening tag
/// is reasoning until something says otherwise.
fn strip_reasoning(raw: &str) -> String {
    let mut out = raw.to_string();
    for (open, close) in REASONING_TAGS {
        let mut stripped = String::with_capacity(out.len());
        let mut rest = out.as_str();
        // `split_once` rather than `find` plus a slice: the crate forbids
        // string indexing, and this needs no indices anyway.
        while let Some((before, after)) = rest.split_once(open) {
            stripped.push_str(before);
            match after.split_once(close) {
                Some((_, tail)) => rest = tail,
                None => {
                    rest = "";
                    break;
                }
            }
        }
        stripped.push_str(rest);
        out = stripped;
    }
    out
}

/// What `dispatch_title` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type TitleQuery = (Entity, &'static RunMetadata);

/// Dispatch system: start the title call for each [`PendingTitle`] run.
///
/// A full pool leaves the marker in place to retry next tick; every other
/// dead end (no settings resource, no resolvable provider/model, provider
/// not registered) drops the marker so the query empties instead of spinning.
pub fn dispatch_title(
    agents: Query<TitleQuery, (With<PendingTitle>, Without<AwaitingTitle>)>,
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
        let Some(permit) = stage.pools.try_acquire(&provider_name, &model) else {
            continue; // pool full - retry next tick
        };

        stage.runtime.spawn(run_title_job(
            TitleJob {
                entity,
                provider,
                provider_name: provider_name.clone(),
                model: model.clone(),
                request: title_request(&meta.task, &provider_name, &model),
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

/// What `collect_title` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type CollectTitleQuery = (
    &'static mut RunMetadata,
    Option<&'static mut crate::persistence::TokenTotals>,
);

/// Collect system: store each finished title into its run's metadata. A
/// provider error or empty reply changes nothing; either way the in-flight
/// marker comes off. What the call billed is counted either way - see the note
/// in the body.
pub fn collect_title(
    mut results: ResMut<TitleResults>,
    mut agents: Query<CollectTitleQuery, With<AwaitingTitle>>,
    persist: Option<Res<crate::pipeline::PersistenceStage>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((mut meta, mut totals)) = agents.get_mut(outcome.entity) else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        // Counted before the reply is examined. A title the sanitizer rejects
        // was still served and still billed, and a run that reports less
        // because its title came back empty would be reporting the one thing
        // this cannot depend on.
        if let Some(usage) = &outcome.usage {
            crate::inference_usage::record_call(
                totals.as_deref_mut(),
                persist.as_deref(),
                Some(&meta),
                &crate::inference_usage::CallUsage {
                    kind: leviath_core::run_archive::InferenceKind::Title,
                    // No stage of its own: titling runs once at spawn, beside
                    // the run rather than inside any of its stages.
                    stage: "",
                    iteration: 0,
                    provider: &outcome.provider_name,
                    model: &outcome.model,
                    usage,
                },
            );
        }
        // A reply that stopped at the token limit was cut off mid-sentence, so
        // whatever it holds is not a finished title however short it looks.
        // That is the shape a reasoning model returns here: it spends the
        // budget working up to an answer and the call ends before the answer.
        if outcome.finish_reason == Some(leviath_providers::FinishReason::TokenLimit) {
            tracing::debug!(
                run_id = %meta.run_id,
                "title reply hit the token limit; leaving the run untitled"
            );
        } else if let Ok(raw) = outcome.result {
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

    /// A provider whose single call yields a fixed reply or a fixed error,
    /// ending for a fixed reason.
    struct Scripted {
        reply: Result<&'static str, &'static str>,
        finish_reason: leviath_providers::FinishReason,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn infer(
            &self,
            _r: &InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            match self.reply {
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
                    finish_reason: self.finish_reason.clone(),
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
            model_override: None,
        }
    }

    /// A world with the title lane wired over the given provider outcome, plus
    /// the receiver the dispatched job reports into.
    fn build_world(
        reply: Result<&'static str, &'static str>,
        pools: crate::inference_pool::InferencePools,
    ) -> (World, mpsc::UnboundedReceiver<TitleOutcome>) {
        build_world_finishing(reply, leviath_providers::FinishReason::Complete, pools)
    }

    /// The same, for the tests that care how the reply ended.
    fn build_world_finishing(
        reply: Result<&'static str, &'static str>,
        finish_reason: leviath_providers::FinishReason,
        pools: crate::inference_pool::InferencePools,
    ) -> (World, mpsc::UnboundedReceiver<TitleOutcome>) {
        let mut registry = crate::ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(Scripted {
                reply,
                finish_reason,
            }),
        );
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
                provider_name: "mock".to_string(),
                model: "m".to_string(),
                request: title_request("task", "mock", "m"),
                permit: pools.try_acquire("p", "m").expect("free"),
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

    /// A reply that ran out of tokens is refused however title-shaped the text
    /// looks. The model was cut off mid-sentence, so what came back is the
    /// start of something rather than a finished title - and that is precisely
    /// what a reasoning model returns when it spends the budget thinking.
    #[tokio::test]
    async fn a_reply_cut_off_at_the_token_limit_leaves_the_title_unset() {
        let (mut world, mut title_rx) = build_world_finishing(
            Ok("Release notes dig"),
            leviath_providers::FinishReason::TokenLimit,
            default_pools(),
        );
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();

        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));

        run_collect(&mut world);
        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title,
            None,
            "a truncated reply is not a title, however short it is"
        );
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    /// The switch the title call sends is the one its provider understands.
    /// Sending them all would 400: each API rejects the others' spelling.
    #[test]
    fn the_title_request_turns_off_thinking_the_way_each_provider_spells_it() {
        assert_eq!(
            title_request("t", "ollama", "qwen3.8").extra,
            serde_json::json!({ "think": false })
        );
        assert_eq!(
            title_request("t", "openrouter", "deepseek/deepseek-r1").extra,
            serde_json::json!({ "reasoning": { "enabled": false } })
        );
        // Anthropic only thinks when asked, and OpenAI never returns reasoning
        // text, so neither needs a switch and neither is sent one.
        assert_eq!(
            title_request("t", "anthropic", "claude-sonnet-4-6").extra,
            serde_json::Value::Null
        );
        assert_eq!(
            title_request("t", "openai", "gpt-5-mini").extra,
            serde_json::Value::Null
        );
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
            finish_reason: Some(leviath_providers::FinishReason::Complete),
            usage: None,
            provider_name: "mock".to_string(),
            model: "m".to_string(),
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
        let held = pools.try_acquire("p", "m").unwrap();
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
        let req = title_request(&long_task, "openai", "gpt-5-mini");
        assert_eq!(req.model, "gpt-5-mini");
        assert_eq!(req.max_tokens, TITLE_MAX_TOKENS);
        // One message, the task. This used to assert two, pinning the shape
        // that Anthropic rejects - the test agreed with the code and both were
        // wrong, which is how titling shipped broken for the default provider.
        assert_eq!(req.messages.len(), 1);
        let expected: leviath_providers::MessageContent =
            leviath_core::truncate_at_boundary(&long_task, TITLE_TASK_BUDGET)
                .to_string()
                .into();
        assert_eq!(req.messages[0].content, expected);
    }

    #[tokio::test]
    async fn scripted_provider_metadata_is_exercised() {
        // Keep the mock's non-`infer` trait methods measured.
        let p = Scripted {
            reply: Ok("t"),
            finish_reason: leviath_providers::FinishReason::Complete,
        };
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
        // One long line and nothing shorter behind it: no title here.
        let long = "word ".repeat(40);
        assert_eq!(sanitize_title(&long), "");
    }

    /// The one that mattered: the instruction goes in a system *block*, not a
    /// message with `role: "system"`.
    ///
    /// Anthropic's Messages API accepts only `user` and `assistant` roles in
    /// `messages` and rejects anything else with a 400, and it is the default
    /// provider for every blueprint Leviath ships. So the old shape meant no
    /// run ever got a title, and nothing said so: a failed title is
    /// deliberately not worth interrupting a run for, and the reason reached
    /// only a debug log in a daemon whose output goes to /dev/null.
    #[test]
    fn the_title_request_carries_its_instruction_as_a_system_block() {
        let request = title_request("tidy the kitchen", "anthropic", "claude-sonnet-4-6");

        assert_eq!(request.system.len(), 1);
        assert!(request.system[0].text.contains("short title"));
        assert!(
            request.messages.iter().all(|m| m.role != "system"),
            "no provider is obliged to accept a system role in messages"
        );
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
    }

    /// A reasoning model answers after thinking out loud, and the thinking is
    /// not a title. This is the reply shape that actually reached a dashboard:
    /// the first line was prose about the task, and it got displayed.
    #[test]
    fn sanitize_skips_reasoning_and_takes_the_line_that_is_a_title() {
        let reply = "We need to generate a short title for the task. The task: \
                     \"Research leviath.dev and report what the docs cover\" - so \
                     something short and descriptive.\n\nLeviath Docs Coverage";
        assert_eq!(sanitize_title(reply), "Leviath Docs Coverage");

        // The same, wrapped in the tags several models use.
        let tagged = "<think>The user wants a title. Keep it under eight words \
                      and avoid punctuation at the end.</think>\nRetry Backoff \
                      Refactor";
        assert_eq!(sanitize_title(tagged), "Retry Backoff Refactor");

        // An unclosed tag drops what follows: it is all reasoning until
        // something says otherwise, and no title beats a wrong one.
        assert_eq!(sanitize_title("<think>thinking with no end"), "");
    }

    /// A compliant reply is untouched.
    #[test]
    fn sanitize_leaves_an_ordinary_reply_alone() {
        assert_eq!(
            sanitize_title("Tidy the kitchen sink"),
            "Tidy the kitchen sink"
        );
        // Exactly at the cap is still a title.
        let brim = "x".repeat(TITLE_MAX_LEN);
        assert_eq!(sanitize_title(&brim), brim);
    }

    /// The bug this module was rewritten for. A reasoning model that never
    /// reaches its answer returns one unbroken paragraph of working-out, so
    /// there is no short line to prefer - and the old code fell back to the
    /// first line *truncated*, which is how a run came to be titled with the
    /// model's own thinking, cut at exactly the display cap.
    ///
    /// Truncating prose does not make it a title. Nothing is stored, and the
    /// run keeps showing the task the user typed.
    #[test]
    fn sanitize_refuses_a_reply_with_no_line_short_enough_to_be_a_title() {
        let leaked = "We need to generate a short title for the user's request. \
                      The user wants to build a dashboard that shows every run \
                      and its current stage, so the title should say that.";
        assert!(leaked.len() > TITLE_MAX_LEN);
        assert_eq!(sanitize_title(leaked), "");

        // What the old fallback made of it, and what a dashboard displayed:
        // the same prose cut at exactly the cap, which is why the stored title
        // was 80 bytes to the byte. Nothing here is allowed to produce it.
        let truncated = leviath_core::truncate_at_boundary(leaked, TITLE_MAX_LEN);
        assert_eq!(
            truncated,
            "We need to generate a short title for the user's request. The user wants to buil"
        );
        assert_ne!(sanitize_title(leaked), truncated);
    }

    /// The cap is in bytes, so the fitting test has to be too. Counting chars
    /// and cutting bytes let an 80-character CJK title pass the check and then
    /// be sliced a quarter of the way through.
    #[test]
    fn sanitize_measures_the_cap_in_bytes_not_characters() {
        let wide = "字".repeat(TITLE_MAX_LEN);
        assert_eq!(wide.chars().count(), TITLE_MAX_LEN);
        assert!(wide.len() > TITLE_MAX_LEN);
        assert_eq!(sanitize_title(&wide), "");

        // Comfortably inside the cap in bytes, so it is a title.
        let short = "字".repeat(8);
        assert_eq!(sanitize_title(&short), short);
    }

    /// `<think>` is not the only spelling. A local GGUF writes its reasoning
    /// into the reply text and no provider strips it, so the title path is the
    /// only place these can be caught.
    #[test]
    fn sanitize_strips_every_reasoning_tag_spelling() {
        assert_eq!(
            sanitize_title("<thinking>a long deliberation about the task</thinking>\nCache Warmup"),
            "Cache Warmup"
        );
        assert_eq!(
            sanitize_title("<reasoning>weighing the options</reasoning>\nQueue Drain Fix"),
            "Queue Drain Fix"
        );
        // Nested spellings, and an unclosed one still drops what follows it.
        assert_eq!(
            sanitize_title("<thinking>outer <think>inner</think> more</thinking>\nDone"),
            "Done"
        );
        assert_eq!(sanitize_title("<reasoning>no closing tag here"), "");
    }

    /// The title call bills like any other and used to be counted like none:
    /// its outcome channel carried the reply the collector wanted and dropped
    /// the usage nobody read, so a run's reported spend was short by one call
    /// it had definitely made.
    #[test]
    fn a_title_call_is_counted_against_the_run_that_paid_for_it() {
        let mut world = World::new();
        let entity = world
            .spawn((
                metadata(Some("mock/m")),
                AwaitingTitle,
                crate::persistence::TokenTotals::default(),
            ))
            .id();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(TitleOutcome {
            entity,
            result: Ok("Release notes".to_string()),
            finish_reason: Some(leviath_providers::FinishReason::Complete),
            usage: Some(leviath_providers::TokenUsage {
                prompt_tokens: 900,
                completion_tokens: 12,
                cached_tokens: 3,
                cache_write_tokens: 4,
                total_tokens: 912,
            }),
            provider_name: "mock".to_string(),
            model: "m".to_string(),
        })
        .unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world);

        let totals = world
            .get::<crate::persistence::TokenTotals>(entity)
            .expect("totals");
        assert_eq!(totals.prompt_tokens, 900);
        assert_eq!(totals.completion_tokens, 12);
        assert_eq!(totals.cached_tokens, 3);
        assert_eq!(totals.cache_write_tokens, 4);
        // And the reply still lands - counting it did not cost the feature.
        assert_eq!(
            world.get::<RunMetadata>(entity).unwrap().title.as_deref(),
            Some("Release notes")
        );
    }

    /// A reply the sanitizer throws away was still served and still billed.
    /// Counting only titles that survive would make a run's reported cost
    /// depend on whether the model happened to answer usefully.
    #[test]
    fn a_rejected_title_is_still_counted() {
        let mut world = World::new();
        let entity = world
            .spawn((
                metadata(Some("mock/m")),
                AwaitingTitle,
                crate::persistence::TokenTotals::default(),
            ))
            .id();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(TitleOutcome {
            entity,
            // Sanitizes to nothing, so no title is stored.
            result: Ok("   ".to_string()),
            finish_reason: Some(leviath_providers::FinishReason::Complete),
            usage: Some(leviath_providers::TokenUsage {
                prompt_tokens: 500,
                completion_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 501,
            }),
            provider_name: "mock".to_string(),
            model: "m".to_string(),
        })
        .unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world);

        assert!(world.get::<RunMetadata>(entity).unwrap().title.is_none());
        assert_eq!(
            world
                .get::<crate::persistence::TokenTotals>(entity)
                .unwrap()
                .prompt_tokens,
            500
        );
    }

    /// A call that never reached a provider has nothing to attribute. Reporting
    /// a zero-token call would put a point on a token chart for a request that
    /// was never made.
    #[test]
    fn a_failed_title_call_adds_nothing() {
        let mut world = World::new();
        let entity = world
            .spawn((
                metadata(Some("mock/m")),
                AwaitingTitle,
                crate::persistence::TokenTotals::default(),
            ))
            .id();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(TitleOutcome {
            entity,
            result: Err(ProviderError::Other("down".to_string())),
            finish_reason: None,
            usage: None,
            provider_name: "mock".to_string(),
            model: "m".to_string(),
        })
        .unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world);

        let totals = world
            .get::<crate::persistence::TokenTotals>(entity)
            .expect("totals");
        assert_eq!(totals.prompt_tokens, 0);
        assert_eq!(totals.completion_tokens, 0);
    }
}
