//! Run-title generation.
//!
//! The dashboard displays, searches, and persists `RunMetadata.title`; this
//! module is what fills it in. At spawn, the daemon marks an eligible run
//! [`PendingTitle`] and hands it a [`TitleCandidates`] chain; `dispatch_title`
//! makes one cheap LLM call over the task prompt via the `title_bridge` worker,
//! and `collect_title` sanitizes the reply into the metadata. Everything
//! downstream (persistence, dashboard header, run search) already reads the
//! field.
//!
//! Best-effort, in that nothing here ever fails a run: a run with no name shows
//! the task the user typed instead. It is not, however, one shot at one
//! provider any more. The call retries a transient refusal on the dispatch
//! lane's schedule, and a call that fails outright moves to the next candidate
//! - the same chain stage inference fails over along.
//!
//! That last part is the fix for a failure worth describing, because every
//! decision in it was individually reasonable. Titling took the head of the
//! run's model chain, called it once, and on any error wrote the reason to
//! `tracing::debug!` - in a daemon whose stdout goes to `/dev/null`. So when an
//! account went over its limit on one gateway, every stage of every run
//! completed (stage inference retried, then failed over) while the runs
//! themselves came out nameless, with nothing anywhere saying why. The lane now
//! retries, fails over, and records what stopped it in
//! [`RunMetadata::title_error`](crate::persistence::RunMetadata::title_error).
//!
//! Retrying makes a title *later*, which brought a second failure into view: a
//! terminal run is unloaded from memory a pass after it finishes, and a title
//! landing on an unloaded run was dropped, reason and all. A run making two
//! provider calls finishes well inside one title call, so it lost even a single
//! 50ms retry. The host now holds a finished run resident while the title lane
//! still has a live claim on it (see `title_outstanding`), the claim is
//! bounded by [`TITLE_JOB_BUDGET_SECS`] so nothing is held long, and the
//! persistence lane treats a landed name as worth a write - without which the
//! title reached the entity and never reached disk.
//!
//! What still stops the attempt without a second opinion is a call that
//! *completed* and produced nothing usable: an empty reply, or one cut off at
//! the token limit. A reasoning model spends output tokens working up to its
//! answer, so a starved budget returns nothing but working-out - and the
//! OpenAI-shaped parsers promote that into `content` when the answer itself is
//! empty. The title call asks the provider not to think, gives it room in case
//! it does anyway, and stores nothing at all rather than the model's thoughts
//! about what a title should be. Paying a second provider to have that same
//! opinion would buy nothing.

use bevy_ecs::prelude::{
    Commands, Component, Entity, Or, Query, Res, ResMut, Resource, With, Without,
};
use leviath_providers::InferenceRequest;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::components::AgentState;
use crate::persistence::RunMetadata;
use crate::pipeline::{InferenceStage, Providers};
use crate::title_bridge::{TitleJob, TitleOutcome, run_title_job};

/// The `[title]` config, as a world resource (inserted by the daemon at
/// setup). Absent in worlds that never title (tests, `lev run` without it).
#[derive(Resource, Clone)]
pub struct TitleSettings(pub leviath_core::config::TitleConfig);

/// This run wants a title; `dispatch_title` picks it up on the next tick.
/// Inserted at spawn for enabled, root, non-empty-task runs only, always
/// beside a [`TitleCandidates`] naming what to call.
#[derive(Component, Debug, Clone, Copy)]
pub struct PendingTitle;

/// The provider/model pairs the title call may use, best first.
///
/// `dispatch_title` takes the head; a call that *fails* drops it and
/// `collect_title` re-arms [`PendingTitle`] so the next one is tried. Empty
/// means every candidate has been spent.
///
/// A chain rather than one shot at the head of the run's own chain. The stage
/// lane retries and then fails over across the blueprint's whole model list, so
/// an account over its limit on one gateway would otherwise produce runs whose
/// stages all completed and whose names never appeared, the title call having
/// taken the 403 and given up. The chain here is the one stage inference walks,
/// for the same reason: the run has a name to generate and several ways to
/// generate it.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleCandidates(pub Vec<(String, String)>);

/// A title call is in flight, and the Unix second by which it must have
/// reported. Unlike `AwaitingCompaction`, this does not hold the agent out of
/// inference - titling runs alongside the first turn.
///
/// The deadline is what lets the host hold a finished run exactly as long as
/// the answer can still arrive, rather than guessing. It is
/// [`TITLE_JOB_BUDGET_SECS`] past dispatch, which is also what bounds the job
/// itself, so a call cannot outlive the entity that is waiting for it.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct AwaitingTitle(pub i64);

/// How long a run may still be owed its name, measured from when it started.
///
/// A terminal run is unloaded from memory a pass after it finishes, and a title
/// landing on an unloaded run is dropped - reason and all. A run that ends
/// quickly therefore races its own title and wins: a probe making two provider
/// calls beats even a single 50ms retry. So the host holds a finished run
/// resident while the title lane still has a live claim on it, and this is how
/// long that claim lasts.
///
/// Measured from the run's *start* rather than from the moment it finished, so
/// the bound cannot be stretched by a run that ends early: whatever happens, a
/// run is never held past this long after its own beginning. A run that took
/// longer than this has already had its window and is never held at all.
///
/// Long enough to cover a title call that was refused once and backed off,
/// short enough that a finished run is not kept in memory waiting on a nicety.
pub const TITLE_HOLD_SECS: i64 = 30;

/// The whole wall-clock budget for one title call, retries and backoff
/// included.
///
/// The dispatch lane's default is fifteen minutes, which is right for a stage
/// turn that may legitimately think that long and wrong for a run's name: a
/// finished run is held in memory until its call reports, and nothing is worth
/// holding one that long. Capping the job here is what makes the hold
/// bounded - the call reports within this, so the entity waiting for it is
/// still there when it does.
pub const TITLE_JOB_BUDGET_SECS: u64 = 60;

/// Whether the title lane's claim on a run that started at `started_at` has run
/// out. The one place the bound is applied, so the host's hold and
/// [`expire_title_hold`]'s verdict cannot disagree about when it ends.
pub(crate) fn title_hold_expired(started_at: i64, now: i64) -> bool {
    now >= started_at.saturating_add(TITLE_HOLD_SECS)
}

/// Whether a claim on a run's name is still live: a call already out and inside
/// its own deadline, or one still queued inside the run's hold window.
///
/// `awaiting` is [`AwaitingTitle`]'s deadline when a call is in flight. The one
/// place the two cases are distinguished, so the host's hold and
/// [`expire_title_hold`]'s verdict cannot disagree.
pub(crate) fn title_claim_live(awaiting: Option<i64>, started_at: i64, now: i64) -> bool {
    match awaiting {
        // A call already out is held to its own deadline, not the run's. The
        // job is bounded by the same number, so this waits exactly as long as
        // an answer can still arrive - which is the difference between a late
        // title landing and being dropped on the floor with its reason.
        Some(deadline) => now < deadline,
        // Nothing out yet: waiting on a pool slot. Bounded from the run's
        // start, so a run that already had its window is never held.
        None => !title_hold_expired(started_at, now),
    }
}

/// Whether the title lane still has a live claim on `entity`.
///
/// The host asks this before unloading a finished run. Answered here rather
/// than in the host because the markers and the bound are this module's, and a
/// second copy of the rule is a second thing to keep in step.
pub(crate) fn title_outstanding(world: &bevy_ecs::world::World, entity: Entity, now: i64) -> bool {
    let awaiting = world.get::<AwaitingTitle>(entity);
    if awaiting.is_none() && world.get::<PendingTitle>(entity).is_none() {
        return false;
    }
    world
        .get::<RunMetadata>(entity)
        .is_some_and(|meta| title_claim_live(awaiting.map(|a| a.0), meta.started_at, now))
}

/// The receiving end of the title-outcomes channel, as a world resource.
#[derive(Resource)]
pub(crate) struct TitleResults(pub UnboundedReceiver<TitleOutcome>);

/// The sending end, cloned into each spawned title job.
#[derive(Resource)]
pub(crate) struct TitleSink(pub UnboundedSender<TitleOutcome>);

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

/// A resolved stage's own model plus everything it would fail over to, as the
/// `(provider, model)` pairs the title lane speaks.
///
/// The caller has a `ResolvedStage`; this crate's title lane wants pairs, and
/// putting the flattening here rather than at the call site keeps the shape of
/// the chain in one place with the code that walks it.
pub fn stage_pairs(
    provider: &str,
    model: &str,
    fallbacks: &[leviath_core::blueprint::ModelEntry],
) -> Vec<(String, String)> {
    std::iter::once((provider.to_string(), model.to_string()))
        .chain(
            fallbacks
                .iter()
                .map(|e| (e.provider.clone(), e.model.clone())),
        )
        .collect()
}

/// The ordered chain of provider/model pairs the title call may walk, best
/// first, for a run whose entry stage resolved to `stage_candidates`.
///
/// The `[title]` config still decides the head, so a provider or model set
/// there is honoured exactly as before. What follows it is the run's own
/// candidate list - the same chain stage inference fails over along - so a head
/// that turns out to be unusable costs the run one attempt rather than its
/// name.
///
/// The tail also answers the case the head resolver has to refuse: an explicit
/// `[title] provider` naming a *different* provider with no `[title] model` is
/// unguessable on its own, because the run's model name means nothing to
/// another provider. Each candidate here carries its own model, so a run in
/// that shape still gets titled - on its own stage models, one step down.
///
/// A pair already in the chain is dropped rather than repeated: trying the same
/// provider and model twice spends a failover step going nowhere.
pub fn title_chain(
    settings: &leviath_core::config::TitleConfig,
    run_model_label: Option<&str>,
    stage_candidates: &[(String, String)],
) -> Vec<(String, String)> {
    let mut chain: Vec<(String, String)> = Vec::new();
    let head = resolve_title_model(settings, run_model_label);
    for pair in head.into_iter().chain(stage_candidates.iter().cloned()) {
        if !chain.contains(&pair) {
            chain.push(pair);
        }
    }
    chain
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
        // Every model on the Codex route is a reasoning model, so a title
        // call left alone spends its whole 256-token budget thinking and
        // returns nothing. The route rejects `effort: "none"` for a model
        // that has reasoning, so this asks for the least of it instead.
        "codex" => serde_json::json!({
            "reasoning": { "effort": "minimal" },
            "text": { "verbosity": "low" }
        }),
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
            reasoning: None,
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
    // nothing and the run keeps showing its task text. Falling back to the
    // first line *truncated* is not an option: truncating prose does not make
    // it a title, it only hides that this failed.
    let stripped = strip_reasoning(raw);
    // Compared in bytes, which is what the cap is in. Counting chars here and
    // cutting bytes afterwards lets a title of 80 CJK characters pass the check
    // and then be sliced mid-title.
    stripped
        .lines()
        .map(strip_control_tokens)
        .map(|l| l.trim_matches(['"', '\'', '`']).trim().to_string())
        .filter(|l| !l.is_empty())
        .find(|l| l.len() <= TITLE_MAX_LEN && !is_degenerate(l) && !echoes_the_instruction(l))
        .unwrap_or_default()
}

/// Cut a line at the first chat-template control token.
///
/// The length rule above is a length rule, and these are short: `<|end_of|`
/// is 9 bytes and one line, so it sailed through and was rendered to a user as
/// the name of their run. Nothing else in the codebase handles these - a
/// search for `<|` found no other site - because every provider that speaks a
/// real API strips them. A local llama.cpp or a thin OpenAI-compatible gateway
/// in front of a GGUF does not always, and that is exactly the setup a title
/// call is cheap enough to be pointed at.
///
/// Cutting rather than deleting: a control token means the model stopped
/// there, so what follows it is another turn's text, not more of this title.
/// A line that *starts* with one is left empty and skipped.
fn strip_control_tokens(line: &str) -> &str {
    match line.split_once("<|") {
        Some((before, _)) => before.trim(),
        None => line.trim(),
    }
}

/// Whether a line is the model stuck repeating itself.
///
/// "response. response. response." is short, single-line, and passes every
/// other check here. Degenerate output is not a title, and showing one to a
/// user is worse than showing them the task they typed.
///
/// The rule is deliberately blunt: enough words to judge, and almost all of
/// them the same word. A real title reuses a word now and then ("Ship the ship
/// docs"), so this needs a run of them before it will refuse.
fn is_degenerate(line: &str) -> bool {
    let words: Vec<String> = line
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect();
    if words.len() < DEGENERATE_MIN_WORDS {
        return false;
    }
    let unique: std::collections::HashSet<&String> = words.iter().collect();
    unique.len() * 2 <= words.len()
}

/// Below this many words there is not enough of a line to call it a loop:
/// "Ship it, ship it" is a person being emphatic, not a model wedged. Three is
/// the smallest that still catches the reported case ("response. response.
/// response.") while leaving a two-word title alone.
const DEGENERATE_MIN_WORDS: usize = 3;

/// Whether a line is the model narrating [`TITLE_SYSTEM_PROMPT`] back at us.
///
/// A model that is asked for a title and answers "drafting a short title (max
/// 8 words, no quotes)" has described the job instead of doing it. That reply
/// is 46 bytes on one line, so the display cap - the whole basis of the
/// earlier reasoning fix - has nothing to say about it.
///
/// Matched on the instruction's own distinctive pairing rather than a list of
/// words that smell like reasoning. "Title" alone is a legitimate title
/// ("Title the release notes"); "title" next to this prompt's own constraints
/// is the model reading the prompt out loud. Being wrong here costs a title
/// and falls back to the task text, so the bar is set to catch the echo rather
/// than to be certain about it.
fn echoes_the_instruction(line: &str) -> bool {
    let lower = line.to_lowercase();
    if !lower.contains("title") {
        return false;
    }
    INSTRUCTION_TELLS.iter().any(|tell| lower.contains(tell))
}

/// Phrases from [`TITLE_SYSTEM_PROMPT`] that a title has no reason to contain,
/// but a paraphrase of the instruction almost always does.
const INSTRUCTION_TELLS: [&str; 6] = [
    "8 words",
    "eight words",
    "no quotes",
    "no explanation",
    "short title",
    "the given task",
];

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

/// Record why this run has no name, on the run itself.
///
/// A `warn!` *and* a field on the run. The daemon's stdout is `/dev/null`, so a
/// reason that reaches only the log leaves "titling failed" and "titling never
/// ran" identical from outside: the log is for whoever is watching, the field is
/// for everyone who was not.
fn record_title_failure(meta: &mut RunMetadata, reason: String) {
    tracing::warn!(
        run_id = %meta.run_id,
        reason = %reason,
        "could not generate a title for this run; it will show its task text instead"
    );
    meta.title_error = Some(reason);
}

/// The host settings the title lane reads.
///
/// Every field is optional because `lev run` and the tests drive these systems
/// with no daemon behind them. Bundled as one `SystemParam` so the dispatch
/// signature stays about what it queries rather than listing what might be
/// wired - and because the three of them together are one thing: how this host
/// wants titles made.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct TitleServices<'w> {
    /// `[title]`, when the daemon installed it. Absent means no titling.
    pub settings: Option<Res<'w, TitleSettings>>,
    /// The operator's retry schedule, from `[limits]`.
    pub tuning: Option<Res<'w, crate::pipeline::InferenceRetryTuning>>,
    /// The clock deadlines are measured against. Absent outside tests, where
    /// the wall clock is the only sensible answer.
    pub clock: Option<Res<'w, crate::pipeline::StallClock>>,
}

impl TitleServices<'_> {
    /// Unix seconds, from the pinned clock when a test installed one.
    fn now(&self) -> i64 {
        self.clock
            .as_deref()
            .map_or_else(|| chrono::Utc::now().timestamp(), |c| (c.0)())
    }
}

/// What `dispatch_title` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type TitleQuery = (
    Entity,
    &'static mut RunMetadata,
    &'static mut TitleCandidates,
);

/// Dispatch system: start the title call for each [`PendingTitle`] run, on the
/// best candidate still standing.
///
/// A full pool leaves the marker in place to retry next tick, and a candidate
/// naming a provider this daemon never registered is skipped over rather than
/// ending the attempt - it can never be called, and leaving it at the head
/// would stall the whole chain behind a name nothing answers to. Only two
/// things drop the marker without a call: titling being switched off, and a
/// chain with nothing callable left in it.
pub(crate) fn dispatch_title(
    mut agents: Query<TitleQuery, (With<PendingTitle>, Without<AwaitingTitle>)>,
    services: TitleServices,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    sink: Res<TitleSink>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    // Read once: `[title] enabled` is a host setting, not a per-run one, and an
    // operator who turns titling off mid-run means it for every pending call.
    let enabled = services.settings.as_deref().is_some_and(|s| s.0.enabled);
    // The dispatch lane's own schedule, so a title call retries a 429 or a
    // dropped connection exactly the way the run's real inference does.
    // The operator's schedule, cut to the title lane's own budget: a name is
    // not worth the dispatch lane's fifteen-minute patience, and the cut is
    // what keeps the host's hold on a finished run short.
    let budget = std::time::Duration::from_secs(TITLE_JOB_BUDGET_SECS);
    let retry = crate::inference_bridge::RetryPolicy {
        job_timeout: budget,
        max_total_backoff: budget,
        ..crate::pipeline::retry_policy_for(
            None,
            services.tuning.as_deref().copied().unwrap_or_default(),
        )
    };
    let now = services.now();
    for (entity, mut meta, mut chain) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if !enabled {
            tracing::debug!(run_id = %meta.run_id, "titling is switched off; skipping");
            commands.entity(entity).remove::<PendingTitle>();
            continue;
        }
        let picked = loop {
            let Some((provider_name, model)) = chain.0.first().cloned() else {
                break None;
            };
            match providers.0.get(&provider_name) {
                Some(provider) => break Some((provider_name, model, provider)),
                None => {
                    tracing::debug!(
                        run_id = %meta.run_id,
                        provider = %provider_name,
                        "title candidate names an unregistered provider; trying the next"
                    );
                    chain.0.remove(0);
                }
            }
        };
        let Some((provider_name, model, provider)) = picked else {
            record_title_failure(
                &mut meta,
                "no configured provider could serve a title call".to_string(),
            );
            commands.entity(entity).remove::<PendingTitle>();
            continue;
        };
        let Some(permit) = stage.pools.try_acquire(&provider_name, &model) else {
            continue; // pool full - retry next tick
        };
        // Spent only once the call is actually going out, so a tick that only
        // found a full pool does not cost the run a candidate.
        chain.0.remove(0);

        stage.runtime.spawn(run_title_job(
            TitleJob {
                entity,
                provider,
                provider_name: provider_name.clone(),
                model: model.clone(),
                request: title_request(&meta.task, &provider_name, &model),
                permit,
            },
            retry,
            sink.0.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<PendingTitle>()
            // Padded by a second so the deadline the host holds to cannot land
            // fractionally before the job's own, which would unload the run in
            // the instant before its answer arrives.
            .insert(AwaitingTitle(
                now.saturating_add(TITLE_JOB_BUDGET_SECS as i64)
                    .saturating_add(1),
            ));
    }
}

/// What `expire_title_hold` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
/// The runs `expire_title_hold` considers: anything the title lane still has a
/// marker on, either shape.
type TitleOwed = Or<(With<PendingTitle>, With<AwaitingTitle>)>;

type ExpireTitleQuery = (
    Entity,
    &'static mut RunMetadata,
    &'static AgentState,
    Option<&'static AwaitingTitle>,
);

/// Stop waiting for a name a finished run is not going to get in time.
///
/// Only terminal runs are given up on. A live run keeps waiting however long it
/// takes - a busy daemon whose pool is full is the case this whole change
/// exists for, and giving up on it after half a minute would put the original
/// bug back. A finished one is different: the host is holding it in memory for
/// this alone, so the wait has to end, and it ends saying why rather than
/// leaving the silence this run's `title_error` exists to break.
pub(crate) fn expire_title_hold(
    mut agents: Query<ExpireTitleQuery, TitleOwed>,
    services: TitleServices,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let now = services.now();
    for (entity, mut meta, state, awaiting) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let terminal = matches!(
            state.status,
            crate::components::AgentStatus::Complete
                | crate::components::AgentStatus::Error { .. }
                | crate::components::AgentStatus::Cancelled
        );
        if !terminal || title_claim_live(awaiting.map(|a| a.0), meta.started_at, now) {
            continue;
        }
        record_title_failure(
            &mut meta,
            "the run finished before a title could be generated".to_string(),
        );
        commands
            .entity(entity)
            .remove::<PendingTitle>()
            .remove::<AwaitingTitle>();
    }
}

/// What `collect_title` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type CollectTitleQuery = (
    &'static mut RunMetadata,
    Option<&'static mut crate::persistence::TokenTotals>,
    &'static TitleCandidates,
);

/// Collect system: store each finished title into its run's metadata.
///
/// The in-flight marker always comes off, and what the call billed is counted
/// either way - see the note in the body. What happens next depends on how the
/// attempt ended:
///
/// - a usable title: stored, and any reason left by an earlier candidate is
///   cleared, because the run does have a name now;
/// - a *failed call* with candidates left: [`PendingTitle`] goes back on and
///   the next provider is tried, the same way stage inference fails over;
/// - anything else: the reason is recorded on the run.
///
/// A call that completed but produced nothing usable ends the attempt rather
/// than failing over. The provider works and answered; another one would be
/// paying for a second opinion on a prompt that is not in doubt. A call that
/// *failed* says nothing about the title and everything about the route to it,
/// which is exactly what the next candidate is for.
pub(crate) fn collect_title(
    mut results: ResMut<TitleResults>,
    mut agents: Query<CollectTitleQuery, With<AwaitingTitle>>,
    persist: Option<Res<crate::pipeline::PersistenceStage>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((mut meta, mut totals, chain)) = agents.get_mut(outcome.entity) else {
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
                // No ledger: with no stage to name, this call belongs to the run
                // and to no stage of it. Handing one over would match nothing
                // anyway, and asking for it would put a component in this
                // query for the sake of a lookup that cannot hit.
                None,
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
                    pricing: outcome.pricing,
                },
            );
        }
        commands.entity(outcome.entity).remove::<AwaitingTitle>();
        // A failed call is the one outcome another provider can do better on,
        // so it is the one that moves down the chain. Re-arming `PendingTitle`
        // is all it takes: the next tick's dispatch picks up the new head.
        if let Err(err) = &outcome.result {
            if !chain.0.is_empty() {
                tracing::warn!(
                    run_id = %meta.run_id,
                    provider = %outcome.provider_name,
                    model = %outcome.model,
                    error = %err,
                    "title call failed; trying the next candidate provider"
                );
                commands.entity(outcome.entity).insert(PendingTitle);
                continue;
            }
            record_title_failure(
                &mut meta,
                format!("{}/{} failed: {err}", outcome.provider_name, outcome.model),
            );
            continue;
        }
        // A reply that stopped at the token limit was cut off mid-sentence, so
        // whatever it holds is not a finished title however short it looks.
        // That is the shape a reasoning model returns here: it spends the
        // budget working up to an answer and the call ends before the answer.
        if outcome.finish_reason == Some(leviath_providers::FinishReason::TokenLimit) {
            record_title_failure(
                &mut meta,
                format!(
                    "{}/{} ran out of output tokens before finishing a title",
                    outcome.provider_name, outcome.model
                ),
            );
            continue;
        }
        let raw = outcome.result.unwrap_or_default();
        let title = sanitize_title(&raw);
        if title.is_empty() {
            record_title_failure(
                &mut meta,
                // Not "nothing short enough" any more: length is only one of
                // the reasons a reply is refused now, and a run whose reply
                // was a stop token or the instruction read back deserves a
                // reason that is true of it.
                format!(
                    "{}/{} replied with nothing usable as a title",
                    outcome.provider_name, outcome.model
                ),
            );
            continue;
        }
        meta.title = Some(title);
        // An earlier candidate may have left a reason behind. The run has a
        // name now, so the explanation for not having one has to go with it.
        meta.title_error = None;
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
                        reported_cost_usd: None,
                    },
                    finish_reason: self.finish_reason.clone(),
                    reasoning: None,
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
            title_error: None,
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
            stream_inference: true,
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

    /// An in-flight title whose deadline is not what the test is about.
    fn awaiting() -> AwaitingTitle {
        AwaitingTitle(i64::MAX)
    }

    /// `started_at` is 0 in `metadata()`, so these read as "seconds into the
    /// run". A run is held while its claim is live and not a second longer.
    #[test]
    fn a_queued_claim_lives_until_the_hold_runs_out() {
        assert!(title_claim_live(None, 0, TITLE_HOLD_SECS - 1));
        assert!(!title_claim_live(None, 0, TITLE_HOLD_SECS));
        assert!(!title_claim_live(None, 0, TITLE_HOLD_SECS + 1));
        // The window travels with the run, not the clock.
        assert!(title_claim_live(None, 1_000, 1_000 + TITLE_HOLD_SECS - 1));
    }

    /// A call already out answers to its own deadline instead, because the job
    /// behind it is bounded by the same number - waiting any less would throw
    /// away an answer that is still coming.
    #[test]
    fn an_in_flight_claim_lives_to_its_own_deadline() {
        // Well past the run's own hold, yet still live: the call is out.
        assert!(title_claim_live(Some(500), 0, TITLE_HOLD_SECS + 100));
        assert!(title_claim_live(Some(500), 0, 499));
        assert!(!title_claim_live(Some(500), 0, 500));
    }

    #[test]
    fn a_run_with_no_title_marker_is_never_outstanding() {
        let mut world = World::new();
        let e = world.spawn(metadata(Some("mock/m"))).id();
        assert!(!title_outstanding(&world, e, 0));
    }

    /// The host reads this off a live world, so the two marker shapes and the
    /// no-metadata case are exercised through it rather than through the pure
    /// rule alone.
    #[test]
    fn outstanding_reads_both_markers_off_the_world() {
        let mut world = World::new();
        let queued = world.spawn((metadata(Some("mock/m")), PendingTitle)).id();
        assert!(title_outstanding(&world, queued, 0));
        assert!(!title_outstanding(&world, queued, TITLE_HOLD_SECS));

        let flying = world
            .spawn((metadata(Some("mock/m")), AwaitingTitle(500)))
            .id();
        assert!(title_outstanding(&world, flying, TITLE_HOLD_SECS + 1));
        assert!(!title_outstanding(&world, flying, 500));

        // A marker with no run metadata cannot be dated, so it holds nothing.
        let bare = world.spawn(PendingTitle).id();
        assert!(!title_outstanding(&world, bare, 0));
    }

    fn agent_state(status: crate::components::AgentStatus) -> AgentState {
        AgentState {
            agent_id: "a".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    /// A pinned clock, so a boundary test is not a coin toss on a loaded runner.
    fn at(secs: i64) -> crate::pipeline::StallClock {
        // A `fn` pointer, so the resource stays `Copy`; the value is baked in
        // per helper rather than captured.
        match secs {
            0 => crate::pipeline::StallClock(|| 0),
            _ => crate::pipeline::StallClock(|| 10_000),
        }
    }

    fn run_expire(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(expire_title_hold);
        schedule.run(world);
    }

    /// A finished run whose claim has run out stops waiting, and says so. This
    /// is the case the host would otherwise unload in silence.
    #[test]
    fn a_finished_run_past_its_hold_gives_up_out_loud() {
        let mut world = World::new();
        world.insert_resource(at(10_000));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                agent_state(crate::components::AgentStatus::Complete),
            ))
            .id();
        run_expire(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title_error.as_deref(),
            Some("the run finished before a title could be generated")
        );
    }

    /// The guard that keeps the original bug fixed: a run that is still going
    /// waits for its name however long the pool makes it wait. Giving up here
    /// after half a minute is exactly what a busy daemon would trip.
    #[test]
    fn a_running_agent_is_never_given_up_on() {
        let mut world = World::new();
        world.insert_resource(at(10_000));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                agent_state(crate::components::AgentStatus::Active),
            ))
            .id();
        run_expire(&mut world);
        assert!(world.get::<PendingTitle>(e).is_some());
        assert_eq!(world.get::<RunMetadata>(e).unwrap().title_error, None);
    }

    /// A finished run whose call is still out keeps waiting for it, and the
    /// in-flight marker comes off with the answer rather than under it.
    #[test]
    fn a_finished_run_still_waits_for_a_call_that_is_out() {
        let mut world = World::new();
        world.insert_resource(at(0));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                AwaitingTitle(i64::MAX),
                agent_state(crate::components::AgentStatus::Complete),
            ))
            .id();
        run_expire(&mut world);
        assert!(world.get::<AwaitingTitle>(e).is_some());
        assert_eq!(world.get::<RunMetadata>(e).unwrap().title_error, None);
    }

    /// A finished run whose call blew its deadline stops waiting too - the same
    /// verdict, reached through the other marker.
    #[test]
    fn a_finished_run_past_its_call_deadline_gives_up() {
        let mut world = World::new();
        world.insert_resource(at(10_000));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                AwaitingTitle(9_000),
                agent_state(crate::components::AgentStatus::Complete),
            ))
            .id();
        run_expire(&mut world);
        assert!(world.get::<AwaitingTitle>(e).is_none());
        assert!(world.get::<RunMetadata>(e).unwrap().title_error.is_some());
    }

    /// Dispatch stamps the deadline the host holds the run to, so the two
    /// cannot drift apart.
    #[tokio::test]
    async fn dispatch_stamps_the_deadline_it_bounded_the_call_by() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        world.insert_resource(TitleSettings(config(None, None)));
        world.insert_resource(at(0));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();
        run_dispatch(&mut world);
        assert_eq!(
            world.get::<AwaitingTitle>(e).map(|a| a.0),
            Some(TITLE_JOB_BUDGET_SECS as i64 + 1),
            "the job's budget plus the second of slack"
        );
    }

    /// The candidate chain a spawned run would carry, from `(provider, model)`
    /// pairs written the way a test reads them.
    fn chain_of(pairs: &[(&str, &str)]) -> TitleCandidates {
        TitleCandidates(
            pairs
                .iter()
                .map(|(p, m)| (p.to_string(), m.to_string()))
                .collect(),
        )
    }

    /// A provider that fails its first `fail_first` calls and then answers.
    /// The counter is what tells a retry apart from a single attempt: the old
    /// title lane called `infer` exactly once, so a provider having one bad
    /// moment cost the run its name for good.
    struct FlakyThenFine {
        fail_first: std::sync::atomic::AtomicUsize,
        /// Built per call rather than stored: `ProviderError` is not `Clone`.
        error: fn() -> ProviderError,
    }

    #[async_trait::async_trait]
    impl Provider for FlakyThenFine {
        async fn infer(
            &self,
            _r: &InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            let left = self
                .fail_first
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| Some(n.saturating_sub(1)),
                )
                .unwrap_or_default();
            if left > 0 {
                return Err((self.error)());
            }
            Ok(leviath_providers::InferenceResponse {
                content: "Recovered Title".to_string(),
                tool_calls: vec![],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    reported_cost_usd: None,
                },
                finish_reason: leviath_providers::FinishReason::Complete,
                reasoning: None,
            })
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "flaky"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// Trait obligations on the flaky mock; only `infer` matters to the tests
    /// that use it, and an unexercised method is an uncovered region.
    #[tokio::test]
    async fn flaky_provider_metadata_is_exercised() {
        let p = FlakyThenFine {
            fail_first: std::sync::atomic::AtomicUsize::new(1),
            error: || ProviderError::Other("scripted".to_string()),
        };
        // Both arms of `infer`, driven directly: the scripted failure, then the
        // answer behind it.
        let request = title_request("t", "mock", "m");
        assert!(p.infer(&request).await.is_err());
        assert_eq!(
            p.infer(&request)
                .await
                .expect("the second call answers")
                .content,
            "Recovered Title"
        );
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        assert_eq!(p.name(), "flaky");
        let _ = p.capabilities("m");
    }

    /// A retry schedule with no waiting in it, so the test asserts the number
    /// of attempts rather than the length of the backoff.
    fn instant_retry() -> crate::inference_bridge::RetryPolicy {
        crate::inference_bridge::RetryPolicy {
            base_delay: std::time::Duration::ZERO,
            capacity_base_delay: std::time::Duration::ZERO,
            ..crate::inference_bridge::RetryPolicy::default()
        }
    }

    /// The regression this whole change exists for, at the job level: a
    /// provider that refuses once and answers next time now yields a title.
    /// Before, `run_title_job` made a single naked `infer` call, so this run
    /// went untitled and said so only to a debug log in a daemon writing to
    /// `/dev/null`.
    #[tokio::test]
    async fn a_transient_refusal_is_retried_rather_than_losing_the_title() {
        let pools = default_pools();
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_title_job(
            TitleJob {
                entity: bevy_ecs::entity::Entity::PLACEHOLDER,
                provider: Arc::new(FlakyThenFine {
                    fail_first: std::sync::atomic::AtomicUsize::new(2),
                    error: || ProviderError::RateLimitExceeded {
                        retry_after_secs: Some(0),
                    },
                }),
                provider_name: "mock".to_string(),
                model: "m".to_string(),
                request: title_request("task", "mock", "m"),
                permit: pools.try_acquire("p", "m").expect("free"),
            },
            instant_retry(),
            tx,
            Arc::new(Notify::new()),
        )
        .await;
        let outcome = rx.recv().await.expect("an outcome is always reported");
        assert_eq!(
            outcome.result.expect("the third attempt answers"),
            "Recovered Title"
        );
    }

    /// A provider whose window is too small for the title request, and which
    /// records whether the request ever reached it.
    struct Narrow {
        inferred: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl Provider for Narrow {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            self.inferred
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(leviath_providers::InferenceResponse {
                content: "A Title".to_string(),
                tool_calls: vec![],
                tokens_used: leviath_providers::TokenUsage::new(1, 0, 0, 1),
                finish_reason: leviath_providers::FinishReason::Complete,
                reasoning: None,
            })
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            600
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1_000
        }
        fn name(&self) -> &str {
            "narrow"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// The title lane is guarded like every other: a titling request that
    /// would overflow the title model's window (600 counted plus the 512-token
    /// reply budget, against 1,000) is refused before it is sent.
    #[tokio::test]
    async fn the_title_lane_refuses_an_overflowing_request() {
        let provider = Arc::new(Narrow {
            inferred: std::sync::atomic::AtomicBool::new(false),
        });
        assert_eq!(provider.name(), "narrow");
        let _ = provider.capabilities("m");
        let pools = default_pools();
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_title_job(
            TitleJob {
                entity: bevy_ecs::entity::Entity::PLACEHOLDER,
                provider: provider.clone(),
                provider_name: "mock".to_string(),
                model: "m".to_string(),
                // A task long enough that the estimate alone (500 tokens) puts
                // the request on the counting side of the line.
                request: title_request(&"t".repeat(2_000), "mock", "m"),
                permit: pools.try_acquire("p", "m").expect("free"),
            },
            instant_retry(),
            tx,
            Arc::new(Notify::new()),
        )
        .await;
        let outcome = rx.recv().await.expect("an outcome is always reported");
        let err = outcome.result.expect_err("600 + 512 does not fit in 1,000");
        assert_eq!(err.to_string(), "Token limit exceeded: 600 > 1000");
        assert!(
            !provider.inferred.load(std::sync::atomic::Ordering::SeqCst),
            "refused before the call, not after it"
        );
        assert!(outcome.usage.is_none(), "nothing was served");
        // The provider would have answered had the request reached it, so the
        // refusal above is the guard's doing and not the mock's.
        let answered = provider
            .infer(&title_request("task", "mock", "m"))
            .await
            .expect("the mock answers");
        assert_eq!(answered.content, "A Title");
        assert!(provider.inferred.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The other half of the schedule: a permanent refusal is not retried, so
    /// a bad request cannot spend the whole attempt budget restating itself.
    #[tokio::test]
    async fn a_permanent_refusal_stops_at_the_first_attempt() {
        let pools = default_pools();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(FlakyThenFine {
            // More failures than the policy has attempts, so a retrying job
            // would still end in error - only the counter tells them apart.
            fail_first: std::sync::atomic::AtomicUsize::new(99),
            error: || ProviderError::Unavailable {
                reason: leviath_providers::UnavailableReason::Forbidden,
                detail: "key limit exceeded (monthly limit)".to_string(),
            },
        });
        run_title_job(
            TitleJob {
                entity: bevy_ecs::entity::Entity::PLACEHOLDER,
                provider: provider.clone(),
                provider_name: "mock".to_string(),
                model: "m".to_string(),
                request: title_request("task", "mock", "m"),
                permit: pools.try_acquire("p", "m").expect("free"),
            },
            instant_retry(),
            tx,
            Arc::new(Notify::new()),
        )
        .await;
        let outcome = rx.recv().await.expect("an outcome is always reported");
        assert!(outcome.result.is_err());
        assert_eq!(
            provider
                .fail_first
                .load(std::sync::atomic::Ordering::SeqCst),
            98,
            "a 403 is the account's answer, not a blip; one call only"
        );
    }

    /// A world whose `dead` provider always refuses and whose `live` one
    /// always answers - the shape of an account that is over its limit on one
    /// gateway and fine on another.
    fn build_failover_world() -> (World, mpsc::UnboundedReceiver<TitleOutcome>) {
        let (mut world, rx) = build_world(Ok("Live Title"), default_pools());
        let mut registry = crate::ProviderRegistry::new();
        registry.register(
            "dead".to_string(),
            Arc::new(Scripted {
                reply: Err("HTTP 403 key limit exceeded"),
                finish_reason: leviath_providers::FinishReason::Complete,
            }),
        );
        registry.register(
            "live".to_string(),
            Arc::new(Scripted {
                reply: Ok("Live Title"),
                finish_reason: leviath_providers::FinishReason::Complete,
            }),
        );
        world.insert_resource(Providers(registry));
        (world, rx)
    }

    /// The failure this change was reported for: every stage of a run
    /// completed because stage inference failed over past a dead gateway,
    /// while the run itself stayed nameless because the title call took the
    /// same refusal with nowhere to go. It now walks the same chain.
    #[tokio::test]
    async fn a_failed_call_moves_the_title_to_the_next_candidate() {
        let (mut world, mut title_rx) = build_failover_world();
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world
            .spawn((
                metadata(Some("dead/m")),
                PendingTitle,
                chain_of(&[("dead", "m"), ("live", "m2")]),
            ))
            .id();

        // First attempt: the dead gateway, which refuses.
        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        assert!(outcome.result.is_err());
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world);

        // Re-armed rather than abandoned, and pointed at the live candidate.
        assert!(world.get::<PendingTitle>(e).is_some());
        assert!(world.get::<AwaitingTitle>(e).is_none());
        assert!(
            world.get::<RunMetadata>(e).unwrap().title_error.is_none(),
            "a candidate that failed with another still to try is not a verdict"
        );

        // Second attempt: the live one, which answers.
        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("second job reported");
        assert_eq!(outcome.provider_name, "live");
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world);

        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title.as_deref(),
            Some("Live Title")
        );
        assert!(world.get::<RunMetadata>(e).unwrap().title_error.is_none());
    }

    /// A title that lands after an earlier candidate failed clears the reason
    /// that candidate left behind: the run has a name, so an explanation for
    /// not having one is stale.
    #[test]
    fn a_landed_title_clears_an_earlier_failure_reason() {
        let mut world = World::new();
        let mut meta = metadata(Some("mock/m"));
        meta.title_error = Some("dead/m failed: HTTP 403".to_string());
        let e = world
            .spawn((meta, awaiting(), TitleCandidates::default()))
            .id();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(TitleOutcome {
            entity: e,
            result: Ok("Second Time Lucky".to_string()),
            finish_reason: Some(leviath_providers::FinishReason::Complete),
            usage: None,
            provider_name: "live".to_string(),
            model: "m2".to_string(),
            pricing: None,
        })
        .unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world);

        let meta = world.get::<RunMetadata>(e).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Second Time Lucky"));
        assert_eq!(meta.title_error, None);
    }

    /// A stage's own pair leads its failover entries, in blueprint order.
    #[test]
    fn stage_pairs_puts_the_resolved_model_ahead_of_its_fallbacks() {
        let fallbacks = vec![
            leviath_core::blueprint::ModelEntry {
                provider: "openai".to_string(),
                model: "gpt-5-mini".to_string(),
            },
            leviath_core::blueprint::ModelEntry {
                provider: "ollama".to_string(),
                model: "qwen3".to_string(),
            },
        ];
        assert_eq!(
            stage_pairs("anthropic", "claude-x", &fallbacks),
            vec![
                ("anthropic".to_string(), "claude-x".to_string()),
                ("openai".to_string(), "gpt-5-mini".to_string()),
                ("ollama".to_string(), "qwen3".to_string()),
            ]
        );
        assert_eq!(
            stage_pairs("anthropic", "claude-x", &[]),
            vec![("anthropic".to_string(), "claude-x".to_string())],
            "a stage with nothing to fail over to is still one candidate"
        );
    }

    /// `[title]` config still leads, the run's own candidates follow it, and a
    /// pair already in the chain is not repeated.
    #[test]
    fn the_chain_puts_the_configured_pair_ahead_of_the_runs_own() {
        let stage = [
            ("anthropic".to_string(), "claude-x".to_string()),
            ("openai".to_string(), "gpt-5-mini".to_string()),
        ];
        assert_eq!(
            title_chain(&config(Some("openai"), Some("gpt-5-mini")), None, &stage),
            vec![
                ("openai".to_string(), "gpt-5-mini".to_string()),
                ("anthropic".to_string(), "claude-x".to_string()),
            ],
            "the configured pair leads and is not repeated behind itself"
        );
    }

    /// With nothing configured the head resolves from the run's label, which
    /// is the entry stage's own pair - so the chain is just the run's list.
    #[test]
    fn the_chain_is_the_runs_own_list_when_nothing_is_configured() {
        let stage = [
            ("anthropic".to_string(), "claude-x".to_string()),
            ("openrouter".to_string(), "anthropic/claude-x".to_string()),
        ];
        assert_eq!(
            title_chain(&config(None, None), Some("anthropic/claude-x"), &stage),
            vec![
                ("anthropic".to_string(), "claude-x".to_string()),
                ("openrouter".to_string(), "anthropic/claude-x".to_string()),
            ]
        );
    }

    /// The case `resolve_title_model` has to refuse on its own - a `[title]`
    /// provider different from the run's, with no `[title]` model - stops
    /// being a dead end, because each candidate carries its own model name.
    #[test]
    fn an_unguessable_configured_provider_falls_through_to_the_runs_chain() {
        let stage = [("anthropic".to_string(), "claude-x".to_string())];
        assert_eq!(
            resolve_title_model(&config(Some("openai"), None), Some("anthropic/claude-x")),
            None
        );
        assert_eq!(
            title_chain(
                &config(Some("openai"), None),
                Some("anthropic/claude-x"),
                &stage
            ),
            vec![("anthropic".to_string(), "claude-x".to_string())]
        );
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
            crate::inference_bridge::RetryPolicy {
                job_timeout: std::time::Duration::from_millis(5),
                ..crate::inference_bridge::RetryPolicy::default()
            },
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
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();

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
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();

        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        assert!(outcome.result.is_err());
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));

        run_collect(&mut world);
        assert_eq!(world.get::<RunMetadata>(e).unwrap().title, None);
        assert!(world.get::<AwaitingTitle>(e).is_none());
        // With the chain spent, the run says why rather than looking like
        // titling was never asked for.
        let reason = world
            .get::<RunMetadata>(e)
            .unwrap()
            .title_error
            .clone()
            .unwrap_or_default();
        assert_eq!(reason.split(':').next(), Some("mock/m failed"));
        assert!(
            world.get::<PendingTitle>(e).is_none(),
            "nothing left to try"
        );
    }

    #[tokio::test]
    async fn whitespace_reply_leaves_the_title_unset() {
        let (mut world, mut title_rx) = build_world(Ok("  \n \n"), default_pools());
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();

        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(outcome).unwrap();
        world.insert_resource(TitleResults(rx));

        run_collect(&mut world);
        assert_eq!(world.get::<RunMetadata>(e).unwrap().title, None);
        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title_error.as_deref(),
            Some("mock/m replied with nothing usable as a title"),
            "the provider answered, so this is the model's verdict, not the route's"
        );
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
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();

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
        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title_error.as_deref(),
            Some("mock/m ran out of output tokens before finishing a title")
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
            pricing: None,
        })
        .unwrap();
        world.insert_resource(TitleResults(rx));
        run_collect(&mut world); // must not panic
    }

    #[tokio::test]
    async fn dispatch_without_settings_drops_the_marker() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();
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
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert!(world.get::<AwaitingTitle>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_with_no_callable_candidate_records_the_reason() {
        let (mut world, _title_rx) = build_world(Ok("t"), default_pools());
        world.insert_resource(TitleSettings(config(Some("nowhere"), Some("m"))));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("nowhere", "m")]),
            ))
            .id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingTitle>(e).is_none());
        assert!(world.get::<AwaitingTitle>(e).is_none());
        // The unregistered head was skipped, the chain emptied, and the run
        // says why it has no name rather than looking like it was never asked.
        assert!(world.get::<TitleCandidates>(e).unwrap().0.is_empty());
        assert_eq!(
            world.get::<RunMetadata>(e).unwrap().title_error.as_deref(),
            Some("no configured provider could serve a title call")
        );
    }

    /// The title call takes the operator's `[limits]` retry schedule, not a
    /// hardcoded one - the same resource the dispatch lane reads.
    #[tokio::test]
    async fn dispatch_uses_the_configured_retry_schedule() {
        let (mut world, mut title_rx) = build_world(Ok("Configured"), default_pools());
        world.insert_resource(TitleSettings(config(None, None)));
        world.insert_resource(crate::pipeline::InferenceRetryTuning {
            max_attempts: 7,
            base_delay_ms: 3,
        });
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();
        run_dispatch(&mut world);
        let outcome = title_rx.recv().await.expect("job reported");
        assert_eq!(outcome.entity, e);
        assert_eq!(outcome.result.expect("the mock answers"), "Configured");
    }

    #[tokio::test]
    async fn dispatch_retries_while_the_pool_is_full() {
        let mut cfg = crate::inference_pool::InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = crate::inference_pool::InferencePools::new(cfg);
        let held = pools.try_acquire("p", "m").unwrap();
        let (mut world, _title_rx) = build_world(Ok("t"), pools);
        world.insert_resource(TitleSettings(config(None, None)));
        let e = world
            .spawn((
                metadata(Some("mock/m")),
                PendingTitle,
                chain_of(&[("mock", "m")]),
            ))
            .id();

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
        // One message, the task. The instruction rides in a system block, not
        // a second message, because Anthropic rejects any role but user and
        // assistant in `messages`.
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
    /// provider for every blueprint Leviath ships. Getting this wrong costs
    /// every run its name quietly: a failed title is deliberately not worth
    /// interrupting a run for.
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

    /// A reasoning model that never reaches its answer returns one unbroken
    /// paragraph of working-out, so there is no short line to prefer. Falling
    /// back to the first line *truncated* would title the run with the model's
    /// own thinking, cut at exactly the display cap.
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

        // What a truncating fallback makes of it: the same prose cut at
        // exactly the cap. Nothing here is allowed to produce it.
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

    /// Three replies verbatim from a live console, each one a hole the display
    /// cap cannot see.
    ///
    /// All three are short, single-line, and free of reasoning tags, so the
    /// "reasoning is longer than a title" rule has nothing to say about any of
    /// them. Without a separate check they are stored and shown to a user as
    /// the names of their runs.
    #[test]
    fn sanitize_refuses_the_three_replies_that_were_shown_to_a_user() {
        // A chat-template control token, cut short exactly as it appeared.
        assert_eq!(sanitize_title("<|end_of|"), "");
        // The model describing the job instead of doing it.
        assert_eq!(
            sanitize_title("drafting a short title (max 8 words, no quotes)"),
            ""
        );
        // The model wedged.
        assert_eq!(sanitize_title("response. response. response."), "");

        // Every one of them would have passed the cap on its own, which is
        // the point: the cap was the only check there was.
        for reply in [
            "<|end_of|",
            "drafting a short title (max 8 words, no quotes)",
            "response. response. response.",
        ] {
            assert!(reply.len() <= TITLE_MAX_LEN);
        }
    }

    /// A control token ends the turn, so what precedes it is the whole title
    /// and what follows is another turn's text.
    #[test]
    fn sanitize_cuts_a_title_at_a_control_token_rather_than_dropping_it() {
        assert_eq!(
            sanitize_title("Cache Warmup<|end_of_text|>"),
            "Cache Warmup"
        );
        assert_eq!(
            sanitize_title("Queue Drain Fix <|eot_id|>"),
            "Queue Drain Fix"
        );
        assert_eq!(
            sanitize_title("Ship the parser<|im_end|>\nnot this line"),
            "Ship the parser"
        );
        // Nothing before the token means nothing to show, so the next line
        // gets its turn rather than the run being left with an empty name.
        assert_eq!(
            sanitize_title("<|endoftext|>\nReal Title Here"),
            "Real Title Here"
        );
    }

    /// The repetition check has to leave ordinary titles alone. A person
    /// writes a repeated word on purpose often enough that refusing on any
    /// repeat would cost more titles than it saves.
    #[test]
    fn sanitize_allows_a_title_that_merely_repeats_a_word() {
        assert_eq!(sanitize_title("Ship the ship docs"), "Ship the ship docs");
        assert_eq!(sanitize_title("Run run"), "Run run");
        assert_eq!(
            sanitize_title("Fix the fix that broke the fix"),
            "Fix the fix that broke the fix"
        );
    }

    /// "Title" is a perfectly good word for a title to contain. Only the
    /// instruction's own constraints alongside it mean the model is reading
    /// the prompt out loud.
    #[test]
    fn sanitize_allows_a_title_that_is_genuinely_about_titles() {
        assert_eq!(
            sanitize_title("Title the release notes"),
            "Title the release notes"
        );
        assert_eq!(
            sanitize_title("Fix run title truncation"),
            "Fix run title truncation"
        );
        // And still refuses the paraphrases, whichever way they are worded.
        assert_eq!(sanitize_title("A short title, at most eight words"), "");
        assert_eq!(sanitize_title("Writing a title for the given task"), "");
        assert_eq!(sanitize_title("Title: no explanation, no quotes"), "");
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

    /// The title call bills like any other, so its usage has to ride the
    /// outcome channel beside the reply the collector wants. Dropping it leaves
    /// a run's reported spend short by one call it definitely made.
    #[test]
    fn a_title_call_is_counted_against_the_run_that_paid_for_it() {
        let mut world = World::new();
        let entity = world
            .spawn((
                metadata(Some("mock/m")),
                awaiting(),
                TitleCandidates::default(),
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
                reported_cost_usd: None,
            }),
            provider_name: "mock".to_string(),
            model: "m".to_string(),
            pricing: None,
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
                awaiting(),
                TitleCandidates::default(),
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
                reported_cost_usd: None,
            }),
            provider_name: "mock".to_string(),
            model: "m".to_string(),
            pricing: None,
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
                awaiting(),
                TitleCandidates::default(),
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
            pricing: None,
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
