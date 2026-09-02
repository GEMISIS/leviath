//! Stage-exit requirement gates: the systems that hold an agent at a stage
//! boundary until something it owes has arrived - its sub-agents, its required
//! context regions, or its final output.
//!
//! Distinct from `gate.rs`, which is the taint gate on tool output.

use super::*;

/// `requires_children` gate (exclusive, mirrors the fan-out wait): a stage marked
/// `requires_children` may not transition while any of the agent's spawned
/// sub-agents ([`SubAgentChildren`](crate::components::SubAgentChildren)) are
/// still running - the parent is held `Waiting` (`WaitingForChildren`) and
/// resumes (re-inserting `ResolveTransition`, back to `Active`) once every child
/// is terminal.
pub(crate) fn gate_requires_children(world: &mut World) {
    crate::tick_scope::clear();
    use crate::components::SubAgentChildren;

    // Hold: transitioning agents whose stage requires children that aren't done.
    // `&AgentState` in the query guarantees the later `.expect()` never fires.
    let mut candidates: Vec<(Entity, Vec<Entity>)> = Vec::new();
    {
        let mut q = world.query_filtered::<(
            Entity,
            &AgentBlueprint,
            &StageCursor,
            &SubAgentChildren,
            &AgentState,
        ), With<ResolveTransition>>();
        for (e, bp, cursor, children, _) in q.iter(world) {
            if bp.0.stages[cursor.index].requires_children {
                candidates.push((e, children.children.clone()));
            }
        }
    }
    for (entity, children) in candidates {
        crate::tick_scope::enter(entity);
        let pending = children.iter().any(|&c| {
            world
                .get::<AgentState>(c)
                .is_some_and(|s| !is_terminal_status(&s.status))
        });
        if pending {
            world
                .entity_mut(entity)
                .remove::<ResolveTransition>()
                .insert(WaitingForChildren);
            world
                .get_mut::<AgentState>(entity)
                .expect("held agent has AgentState")
                .status = AgentStatus::Waiting;
        }
    }

    // Resume: held agents whose children have all finished.
    crate::tick_scope::clear();
    let mut waiting: Vec<(Entity, Vec<Entity>)> = Vec::new();
    {
        let mut q = world.query_filtered::<
            (Entity, Option<&SubAgentChildren>, &AgentState),
            With<WaitingForChildren>,
        >();
        for (e, children, _) in q.iter(world) {
            waiting.push((e, children.map(|c| c.children.clone()).unwrap_or_default()));
        }
    }
    for (entity, children) in waiting {
        crate::tick_scope::enter(entity);
        let all_done = children.iter().all(|&c| {
            world
                .get::<AgentState>(c)
                .is_none_or(|s| is_terminal_status(&s.status))
        });
        if all_done {
            world
                .entity_mut(entity)
                .remove::<WaitingForChildren>()
                .insert(ResolveTransition);
            world
                .get_mut::<AgentState>(entity)
                .expect("waiting agent has AgentState")
                .status = AgentStatus::Active;
        }
    }
}

/// Default re-entry cap for required-region gating: how many times a stage is
/// re-run to populate an empty `required` region before proceeding anyway (with a
/// warning). Overridable per stage via `max_revisits`.
pub(crate) const DEFAULT_REQUIRED_REENTRY_CAP: usize = 3;

/// Counts how many times the current stage has been re-run to satisfy required
/// context regions. Absent ⇒ 0; reset when a new stage is entered.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct RequiredReentries(pub usize);

/// Required regions (from the stage's effective layout) still empty at stage end,
/// as `(name, optional custom message)`. Empty when the stage has no
/// context-writing tool (gating a stage that can't populate the region would loop
/// pointlessly). Ported from the imperative `unmet_required_regions`.
pub(crate) fn unmet_required_regions(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    window: &ContextWindow,
) -> Vec<(String, Option<String>)> {
    let can_write = stage.grants_all_builtins()
        || stage
            .available_tools
            .iter()
            .any(|t| t == "context_write" || t == "context_append");
    if !can_write {
        return Vec::new();
    }
    let layout = stage
        .context_layout
        .as_ref()
        .unwrap_or(&blueprint.context_layout);
    layout
        .regions
        .iter()
        .filter(|r| r.required)
        // Caller-input regions are validated (and seeded) at spawn, not written
        // by the agent - skip them here so this gate never nags the agent to
        // populate a slot the caller owns.
        .filter(|r| {
            !matches!(
                r.seed,
                Some(leviath_core::layout::RegionSeed::CallerInput { .. })
            )
        })
        .filter(|r| {
            window
                .get_region(&r.name)
                .map(|reg| reg.content.is_empty())
                .unwrap_or(true)
        })
        .map(|r| (r.name.clone(), r.required_message.clone()))
        .collect()
}

/// Inject a `[System]` nudge into the conversation region for each unmet required
/// region, so the stage re-run tells the agent exactly what to populate. A custom
/// `required_message` may name the region via a `{region}` placeholder; the
/// generated default is built through the same substitution.
pub(crate) fn inject_required_region_nudges(
    window: &mut ContextWindow,
    unmet: &[(String, Option<String>)],
) {
    const DEFAULT_REQUIRED_MESSAGE: &str = "Required context region '{region}' is still empty. \
         You must populate it (e.g. via context_write with region=\"{region}\") before this \
         stage can complete.";
    for (name, msg) in unmet {
        let text = leviath_core::text::interpolate(
            msg.as_deref().unwrap_or(DEFAULT_REQUIRED_MESSAGE),
            &[("region", name)],
        );
        crate::pipeline::response::inject_system_nudge(window, &text);
    }
}

/// What `require_context_regions` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ContextRegionQuery = (
    Entity,
    &'static AgentBlueprint,
    &'static StageCursor,
    &'static mut ContextWindow,
    Option<&'static RequiredReentries>,
    Option<&'static StageOutcome>,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
);

/// Required-region gate: before a normally-completed stage transitions, if it can
/// write context and a `required` region is still empty, inject a nudge and re-run
/// the stage (loop back to `ReadyToInfer`) instead of transitioning - bounded by
/// the stage's `max_revisits` (or a default cap), after which
/// it proceeds with a warning. Skipped when the stage ended on an error / max-iter
/// outcome (those transitions take precedence). Ported from the imperative gate.
pub(crate) fn require_context_regions(
    mut agents: Query<ContextRegionQuery, With<ResolveTransition>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, bp, cursor, mut window, reentries, outcome, mut flags) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        // A stage that gave up is not the last word: `sources_index` abandoned in
        // `gather` can be written by `analyze`, and then the artifact exists.
        // Reported as still missing, it tells a reader something false - one run
        // finished with a fifty-citation bibliography while warning that the
        // region had never been written. The moment stays in the log; this list
        // is what is actually missing, checked at every stage boundary.
        // A stage that gave up is not the last word: `sources_index` abandoned in
        // `gather` can be written by `analyze`, and then the artifact exists.
        // Reported as still missing, it tells a reader something false - one run
        // finished with a fifty-citation bibliography while warning that the
        // region had never been written. The moment stays in the log; this list
        // is what is actually missing, checked at every stage boundary.
        if let Some(flags) = flags.as_mut() {
            flags.0.required_regions_abandoned.retain(|name| {
                window
                    .get_region(name)
                    .is_none_or(|region| region.content.is_empty())
            });
        }
        if outcome.is_some() {
            continue; // error / max-iterations transition takes precedence
        }
        let stage = &bp.0.stages[cursor.index];
        let unmet = unmet_required_regions(&bp.0, stage, &window);
        if unmet.is_empty() {
            continue;
        }
        let cap = stage.max_revisits.unwrap_or(DEFAULT_REQUIRED_REENTRY_CAP);
        let round = reentries.map_or(0, |r| r.0);
        if round >= cap {
            let names: Vec<&str> = unmet.iter().map(|(n, _)| n.as_str()).collect();
            tracing::warn!(
                stage = %stage.name,
                regions = ?names,
                attempts = cap,
                "required context regions still empty after re-run attempts; proceeding"
            );
            // Recorded as well as logged. A log line is not readable after the
            // fact, so without this "the agent wrote its plan" and "we asked
            // twice and moved on" both finish `complete` and nothing
            // downstream can tell them apart.
            if let Some(mut flags) = flags {
                for name in &names {
                    if !flags
                        .0
                        .required_regions_abandoned
                        .iter()
                        .any(|seen| seen == name)
                    {
                        flags.0.required_regions_abandoned.push((*name).to_string());
                    }
                }
            }
            continue; // proceed with the transition despite the unmet regions
        }
        inject_required_region_nudges(&mut window, &unmet);
        commands
            .entity(entity)
            .remove::<ResolveTransition>()
            .insert(ReadyToInfer)
            .insert(RequiredReentries(round + 1));
    }
}

/// Counts how many times the current stage has been re-run for a final output
/// it was required to produce. Absent ⇒ 0; reset when a new stage is entered.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct OutputReentries(pub usize);

/// The nudge a stage gets when it finishes without the output it owes.
///
/// Names the tool rather than describing it, because the description the model
/// already has carries the shape; what it missed was that the call is not
/// optional.
const MISSING_OUTPUT_NUDGE: &str = "This stage is not finished: you have not called `submit_output`. Whatever you wrote to \
     files or to context is not what the caller receives - only the final output is. Call \
     `submit_output` now with your answer.";

/// What `require_final_output` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type FinalOutputQuery = (
    Entity,
    &'static AgentBlueprint,
    &'static StageCursor,
    &'static AgentState,
    &'static mut ContextWindow,
    Option<&'static OutputReentries>,
    Option<&'static StageOutcome>,
    Option<&'static crate::persistence::FinalOutput>,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
);

/// Required-output gate: hold a stage that owes a final output and has not
/// submitted one, nudge it, and re-run - bounded, then give up loudly.
///
/// The same shape as [`require_context_regions`] and the edge gate's
/// `require_modifications`, and deliberately so: a missing output never strands
/// a run. When the re-entry budget is spent the transition proceeds and the run
/// records `output_forced`, so a caller reading `meta.json` can tell "no answer
/// because the agent never gave one" from "no answer because nobody asked".
///
/// Skipped when the stage ended on an error or max-iterations outcome, which
/// take precedence: an agent that already failed should follow its error edge
/// rather than be told to summarise.
///
/// Whether *this stage* submitted is the question, not whether the run holds an
/// output from anywhere. A blueprint whose worker stage submits and whose later
/// summary stage also must would otherwise let the summary coast on the
/// worker's answer.
pub(crate) fn require_final_output(
    mut agents: Query<FinalOutputQuery, With<ResolveTransition>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, bp, cursor, state, mut window, reentries, outcome, submitted, mut flags) in
        agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        let stage = &bp.0.stages[cursor.index];
        if !stage.require_output {
            continue;
        }
        // An output carried in from an earlier stage does not discharge this
        // stage's obligation.
        if submitted.is_some_and(|o| o.0.stage == state.current_stage) {
            continue;
        }
        // An error or max-iterations transition takes precedence over the nudge:
        // the stage is already ending and holding it here would fight that. The
        // *flag* still has to be honest though. A model that cannot satisfy its
        // validator burns every iteration retrying and leaves on that path, so
        // this is the ordinary way a required output goes missing, not an edge
        // case. Left unrecorded the run reports `output_forced: 0`, which reads
        // as "nothing was required" rather than "the requirement went unmet".
        if outcome.is_some() {
            tracing::warn!(
                stage = %stage.name,
                "stage ended without its required final output"
            );
            if let Some(flags) = flags.as_mut() {
                flags.0.output_forced += 1;
            }
            continue;
        }
        // Its own budget, not the stage's `max_revisits`. Those are different
        // questions - "how many times may the graph re-enter this stage" and
        // "how many times do we nudge a model that owes an answer" - and
        // borrowing the first for the second made a routing setting silently
        // multiply an inference bill. Each retry re-sends the whole stage
        // context, and an output stage runs last, when that context is at its
        // largest: an agent with `max_revisits = 10` billed ten full prompts to
        // fail to say one word.
        let cap = leviath_core::blueprint::DEFAULT_OUTPUT_REENTRY_CAP;
        let round = reentries.map_or(0, |r| r.0);
        if round >= cap {
            tracing::warn!(
                stage = %stage.name,
                attempts = cap,
                "stage never produced its required final output; proceeding without one"
            );
            if let Some(flags) = flags.as_mut() {
                flags.0.output_forced += 1;
            }
            continue; // proceed rather than strand the run
        }
        crate::pipeline::response::inject_system_nudge(&mut window, MISSING_OUTPUT_NUDGE);
        commands
            .entity(entity)
            .remove::<ResolveTransition>()
            .insert(ReadyToInfer)
            .insert(OutputReentries(round + 1));
    }
}

/// What a fan-out stage is told when it tries to leave without fanning out.
const MISSING_FAN_OUT_NUDGE: &str = "This stage has not started its workers yet. Call `fan_out` \
     with the work items - that is the whole job of this stage, and nothing \
     downstream runs until you do. If there is genuinely nothing to hand out, \
     call it with an empty `items` array to say so.";

/// Times this stage has been asked again to fan out.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FanOutReentries(pub usize);

/// What [`require_fan_out`] selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about lifetimes:
/// the borrow is bound when the query is fetched.
type RequireFanOutQuery = (
    Entity,
    &'static AgentBlueprint,
    &'static StageCursor,
    &'static mut ContextWindow,
    Option<&'static FanOutReentries>,
    Option<&'static StageOutcome>,
    Option<&'static crate::fanout::FannedOut>,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
);

/// Hold a `fan_out` stage that is trying to transition without having fanned
/// out, and ask it again.
///
/// A fan-out stage exists to start workers. Before this, a stage whose model
/// answered in prose simply transitioned, and the merge stage ran on nothing -
/// silently, because "no workers ran" and "there was nothing to hand out" look
/// identical from the far side. That was the quiet half of the failure that
/// killed a `deep-researcher` run; the loud half was the runtime ending the run
/// over it.
///
/// Neither now. The stage is asked again a bounded number of times and then let
/// through with `splits_degraded` recorded, which is the same shape
/// [`require_final_output`] uses for a missing answer: never strand a run over a
/// thing the model would not do, and never let it pass for success either.
pub(crate) fn require_fan_out(
    mut agents: Query<RequireFanOutQuery, With<ResolveTransition>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, bp, cursor, mut window, reentries, outcome, fanned_out, mut flags) in
        agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        let stage = &bp.0.stages[cursor.index];
        let leviath_core::blueprint::StageMode::FanOut { config } = &stage.mode else {
            continue;
        };
        // The stage's own budget when it set one; a small or local model may
        // need more than a nudge, and `0` says to let it through on the first
        // refusal rather than pay for retries that will not land.
        let cap = config
            .max_attempts
            .unwrap_or(leviath_core::blueprint::DEFAULT_FAN_OUT_ATTEMPTS);
        // Set by every accepted call and cleared on stage entry, so its
        // presence means "this entry has already fanned out" rather than "this
        // run has, at some point".
        if fanned_out.is_some() {
            continue;
        }
        // A stage already ending on an error or its iteration cap is not held
        // here - that transition takes precedence - but the flag still has to be
        // honest about what the merge stage is about to work from.
        if outcome.is_some() {
            tracing::warn!(stage = %stage.name, "fan_out stage ended without starting any workers");
            if let Some(flags) = flags.as_mut() {
                flags.0.splits_degraded += 1;
            }
            continue;
        }
        let round = reentries.map_or(0, |r| r.0);
        if round >= cap {
            tracing::warn!(
                stage = %stage.name,
                attempts = cap,
                "fan_out stage never started its workers; proceeding without them"
            );
            crate::pipeline::note_unusable_split(
                &mut window,
                &stage.name,
                "the stage never called `fan_out`",
            );
            if let Some(flags) = flags.as_mut() {
                flags.0.splits_degraded += 1;
            }
            continue; // proceed rather than strand the run
        }
        crate::pipeline::response::inject_system_nudge(&mut window, MISSING_FAN_OUT_NUDGE);
        commands
            .entity(entity)
            .remove::<ResolveTransition>()
            .insert(ReadyToInfer)
            .insert(FanOutReentries(round + 1));
    }
}

/// What a chosen edge's gate says about the transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateDecision {
    /// The gate is satisfied (or absent) - follow the edge.
    Pass,
    /// The gate is unsatisfied but out of re-run budget - follow the edge and
    /// record it in the run's flags so the run explains itself afterwards.
    Forced,
    /// Hold the agent in this stage and show it this nudge.
    Block(String),
}
