//! Declarative stage-boundary interaction points (`StageMode::InteractivePoints`).
//!
//! Unlike the model-driven `ask_user_*` / `edit_document` tools (which fire only
//! if the model chooses to call them — see [`crate::dynamic_interaction`]), an
//! interaction point is declared statically in the blueprint and fired by the
//! framework at the stage boundary, *always*, before the stage may transition.
//! The canonical example is `plan_approval`: after the plan stage produces a
//! plan, the user is shown a choice — approve / revise / edit / abort — and the
//! answer deterministically routes what happens next.
//!
//! This is a first-class ECS lane, mirroring the transition-choice lane:
//! - [`gate_interaction_points`] intercepts a would-be transition
//!   ([`ResolveTransition`]) for an interactive-points stage and instead marks the
//!   agent [`ReadyForInteractionPoint`].
//! - [`dispatch_interaction_point`] spawns an async task that asks through the
//!   shared [`InteractionHub`] (so the dashboard surfaces the prompt via
//!   [`reflect_interaction_status`](crate::pipeline::reflect_interaction_status)),
//!   resolves the answer, and reports a [`PointOutcome`] on the lane.
//! - [`collect_interaction_point`] applies the outcome: approve ⇒ proceed to the
//!   transition, abort ⇒ cancel the run, a directive ⇒ inject it and re-run
//!   inference in-stage, an edit ⇒ inject the edited text and re-present the
//!   point. Directive/edit loops are bounded by [`MAX_REVISION_ROUNDS`].
//!
//! The routing is deterministic (code); only the input capture is a user
//! interaction — faithfully porting the deleted imperative
//! `run_interactive_points_stage`.

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::blueprint::{InteractionPoint, InteractionStyle, StageMode};
use leviath_core::interaction::{InteractionRequest, InteractionResponse};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::components::{AgentState, AgentStatus, ContextWindow, InferenceResult};
use crate::dynamic_interaction::InteractionBackend;
use crate::interaction_hub::InteractionHub;
use crate::pipeline::{
    AgentBlueprint, ReadyToInfer, ResolveTransition, StageCursor, StageIoBuffer,
};

/// Maximum directive/edit revision rounds at one interaction point before the
/// stage proceeds regardless, so a revise/edit loop can never run forever.
pub const MAX_REVISION_ROUNDS: usize = 4;

// ─── Components ──────────────────────────────────────────────────────────────

/// The agent's current stage is done and has an unsatisfied interaction point;
/// the dispatch system should ask it. (Set by the gate or by an edit re-present.)
#[derive(Component, Debug, Clone, Copy)]
pub struct ReadyForInteractionPoint;

/// An interaction point is in flight (its request is open in the hub); the
/// collect system applies the answer when the lane reports it.
#[derive(Component, Debug, Clone, Copy)]
pub struct AwaitingInteractionPoint;

/// Which interaction point (index into the stage's `points`) the agent is on.
/// Absent ⇒ 0. Advanced on approve; reset when a new stage is entered.
#[derive(Component, Debug, Clone, Copy)]
pub struct InteractionPointCursor(pub usize);

/// How many directive/edit revision rounds have been taken at the current point.
/// Absent ⇒ 0. Reset on approve (advancing points) and on entering a new stage.
#[derive(Component, Debug, Clone, Copy)]
pub struct InteractionPointRounds(pub usize);

// ─── Lane plumbing ───────────────────────────────────────────────────────────

/// What the user's answer resolved to, routed deterministically from the option
/// label. Carries the text the collect system must inject into context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointOutcome {
    /// A plain option (no directive/abort/edit) ⇒ complete the point.
    Approve { user_text: String },
    /// An abort option ⇒ cancel the run immediately.
    Abort,
    /// A directive option ⇒ inject the directive and re-run inference in-stage.
    Directive {
        user_text: String,
        directive: String,
    },
    /// An edit option ⇒ inject the user's edited text and re-present the point.
    Edit { user_text: String, edited: String },
}

/// One resolved interaction-point answer, reported on the lane.
pub struct InteractionPointOutcome {
    /// The agent the answer is for.
    pub entity: Entity,
    /// The routed decision.
    pub decision: PointOutcome,
}

/// The sending side of the interaction-point lane + the handle/wake needed to
/// drive the async ask task, as a world resource.
#[derive(Resource)]
pub struct InteractionPointStage {
    /// Where resolved outcomes are reported.
    pub outcomes: UnboundedSender<InteractionPointOutcome>,
    /// Wakes the tick loop when an outcome lands.
    pub wake: Arc<Notify>,
    /// Runtime the ask task is spawned onto.
    pub runtime: Handle,
}

/// The receiving side of the interaction-point lane, for the collect system.
#[derive(Resource)]
pub struct InteractionPointResults(pub UnboundedReceiver<InteractionPointOutcome>);

// ─── Pure routing helpers (ported from the deleted imperative stage loop) ─────

/// Normalize an option label for matching: fold Unicode dashes to ASCII `-` and
/// collapse whitespace, so `"Revise — I'll…"` matches regardless of dash style.
fn normalize_for_followup(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2014}' | '\u{2013}' | '\u{2212}' | '\u{2015}' => '-',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `user_text` matches one of `candidates` (exact first, then normalized).
fn option_matches(candidates: &[String], user_text: &str) -> bool {
    if candidates.iter().any(|o| o == user_text) {
        return true;
    }
    let normalized = normalize_for_followup(user_text);
    candidates
        .iter()
        .any(|o| normalize_for_followup(o) == normalized)
}

/// Look up a directive by option label (exact first, then normalized).
fn lookup_directive<'a>(
    directives: &'a HashMap<String, String>,
    user_text: &str,
) -> Option<&'a str> {
    if let Some(d) = directives.get(user_text) {
        return Some(d.as_str());
    }
    let normalized = normalize_for_followup(user_text);
    directives
        .iter()
        .find(|(k, _)| normalize_for_followup(k) == normalized)
        .map(|(_, d)| d.as_str())
}

/// Build the interaction request for a point in its declared style.
fn build_point_request(point: &InteractionPoint, id: String) -> InteractionRequest {
    match point.style {
        InteractionStyle::MultipleChoice => InteractionRequest::multiple_choice(
            id,
            &point.prompt,
            point.options.clone(),
            &point.name,
        ),
        InteractionStyle::Confirm => InteractionRequest::confirm(id, &point.prompt, &point.name),
        InteractionStyle::FreeText => {
            InteractionRequest::free_text(id, &point.prompt, &point.name, point.required)
        }
    }
}

/// Resolve a response to the selected option label / free text: a choice index
/// maps through `options`, otherwise the free-text value (empty if none).
fn resolve_answer(resp: &InteractionResponse, options: &[String]) -> String {
    if let Some(opt) = resp.choice_index.and_then(|i| options.get(i)) {
        return opt.clone();
    }
    resp.value.clone().unwrap_or_default()
}

/// Route a resolved answer to a [`PointOutcome`] (pure; the edit branch's second
/// ask is done by the caller, which knows the edited text).
fn route_answer(point: &InteractionPoint, user_text: String) -> Routed {
    if option_matches(&point.abort_options, &user_text) {
        Routed::Abort
    } else if option_matches(&point.edit_options, &user_text) {
        Routed::Edit { user_text }
    } else if let Some(directive) = lookup_directive(&point.directives, &user_text) {
        Routed::Directive {
            user_text,
            directive: directive.to_string(),
        }
    } else {
        Routed::Approve { user_text }
    }
}

/// Intermediate routing result before the edit branch's second ask.
#[derive(Debug, PartialEq, Eq)]
enum Routed {
    Approve {
        user_text: String,
    },
    Abort,
    Directive {
        user_text: String,
        directive: String,
    },
    Edit {
        user_text: String,
    },
}

// ─── The async ask task ──────────────────────────────────────────────────────

/// Ask an interaction point through the hub, resolve + route the answer (doing
/// the edit branch's second "edit this text" ask when needed), and report the
/// [`PointOutcome`] on the lane, waking the tick loop.
#[allow(clippy::too_many_arguments)]
async fn run_interaction_point(
    entity: Entity,
    hub: InteractionHub,
    agent_id: String,
    point: InteractionPoint,
    body: String,
    round: usize,
    outcomes: UnboundedSender<InteractionPointOutcome>,
    wake: Arc<Notify>,
) {
    // Request ids are prefixed with the run id so concurrent runs at the same
    // point (same name/round) never collide in the shared hub.
    let ask_id = format!("{agent_id}-point-{}-{round}", point.name);
    let backend = hub.backend_for(agent_id);
    let req = build_point_request(&point, ask_id.clone());
    let resp = backend.ask(req).await;
    let user_text = resolve_answer(&resp, &point.options);

    let decision = match route_answer(&point, user_text) {
        Routed::Approve { user_text } => PointOutcome::Approve { user_text },
        Routed::Abort => PointOutcome::Abort,
        Routed::Directive {
            user_text,
            directive,
        } => PointOutcome::Directive {
            user_text,
            directive,
        },
        Routed::Edit { user_text } => {
            let edit_req = InteractionRequest::edit_text(
                format!("{ask_id}-edit"),
                "Edit the text below, then submit your changes:",
                &point.name,
                body,
            );
            let edited = backend.ask(edit_req).await.value.unwrap_or_default();
            PointOutcome::Edit { user_text, edited }
        }
    };

    let _ = outcomes.send(InteractionPointOutcome { entity, decision });
    wake.notify_one();
}

// ─── Systems ─────────────────────────────────────────────────────────────────

/// Read the interaction points of an agent's current stage, or `None` if the
/// stage isn't an interactive-points stage.
fn stage_points<'a>(
    bp: &'a AgentBlueprint,
    cursor: &StageCursor,
) -> Option<&'a [InteractionPoint]> {
    match &bp.0.stages[cursor.index].mode {
        StageMode::InteractivePoints { points } => Some(points),
        _ => None,
    }
}

/// Gate: intercept a would-be transition for an interactive-points stage whose
/// points aren't all satisfied yet, routing the agent to the interaction-point
/// lane instead. Stages with no points, or whose point cursor is past the end
/// (all approved), fall through to the normal transition.
#[allow(clippy::type_complexity)]
pub fn gate_interaction_points(
    agents: Query<
        (
            Entity,
            &AgentBlueprint,
            &StageCursor,
            Option<&InteractionPointCursor>,
        ),
        With<ResolveTransition>,
    >,
    mut commands: Commands,
) {
    for (entity, bp, cursor, pc) in agents.iter() {
        let Some(points) = stage_points(bp, cursor) else {
            continue;
        };
        let idx = pc.map_or(0, |c| c.0);
        if points.is_empty() || idx >= points.len() {
            continue; // nothing to ask ⇒ let the transition proceed
        }
        commands
            .entity(entity)
            .remove::<ResolveTransition>()
            .insert(ReadyForInteractionPoint);
    }
}

/// Dispatch: for each `ReadyForInteractionPoint` agent, spawn the ask task for
/// its current point and move it to `AwaitingInteractionPoint`. No hub (test
/// world) ⇒ no-op; a non-interactive stage ⇒ fall back to the transition.
#[allow(clippy::type_complexity)]
pub fn dispatch_interaction_point(
    agents: Query<
        (
            Entity,
            &AgentState,
            &AgentBlueprint,
            &StageCursor,
            &InferenceResult,
            Option<&InteractionPointCursor>,
            Option<&InteractionPointRounds>,
        ),
        With<ReadyForInteractionPoint>,
    >,
    hub: Option<Res<InteractionHub>>,
    stage: Option<Res<InteractionPointStage>>,
    mut commands: Commands,
) {
    let (Some(hub), Some(stage)) = (hub, stage) else {
        return; // no lane wired (test world)
    };
    for (entity, state, bp, cursor, infer, pc, rounds) in agents.iter() {
        if state.status != AgentStatus::Active {
            continue; // paused / cancelled — don't open a prompt
        }
        let idx = pc.map_or(0, |c| c.0);
        let point = stage_points(bp, cursor).and_then(|p| p.get(idx)).cloned();
        let Some(point) = point else {
            // Stage changed out from under us ⇒ just proceed to the transition.
            commands
                .entity(entity)
                .remove::<ReadyForInteractionPoint>()
                .insert(ResolveTransition);
            continue;
        };
        stage.runtime.spawn(run_interaction_point(
            entity,
            hub.clone(),
            state.agent_id.clone(),
            point,
            infer.response.clone(),
            rounds.map_or(0, |r| r.0),
            stage.outcomes.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<ReadyForInteractionPoint>()
            .insert(AwaitingInteractionPoint);
    }
}

/// Collect: apply each resolved interaction-point outcome — approve advances
/// (or transitions when all points are done), abort cancels, a directive injects
/// the directive and re-infers in-stage, an edit injects the edited text and
/// re-presents; both revision paths are bounded by [`MAX_REVISION_ROUNDS`].
#[allow(clippy::type_complexity)]
pub fn collect_interaction_point(
    mut results: ResMut<InteractionPointResults>,
    mut agents: Query<
        (
            &mut AgentState,
            &mut ContextWindow,
            &AgentBlueprint,
            &StageCursor,
            Option<&InteractionPointCursor>,
            Option<&InteractionPointRounds>,
            Option<&mut StageIoBuffer>,
        ),
        With<AwaitingInteractionPoint>,
    >,
    mut commands: Commands,
) {
    while let Ok(out) = results.0.try_recv() {
        let Ok((mut state, mut window, bp, cursor, pc, rounds, io_buf)) =
            agents.get_mut(out.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        let idx = pc.map_or(0, |c| c.0);
        let round = rounds.map_or(0, |r| r.0);
        let (name, npoints) = match stage_points(bp, cursor) {
            Some(points) => (
                points.get(idx).map(|p| p.name.clone()).unwrap_or_default(),
                points.len(),
            ),
            None => (String::new(), 0),
        };

        let mut e = commands.entity(out.entity);
        e.remove::<AwaitingInteractionPoint>();

        // Mark all points satisfied so the gate lets the transition proceed
        // (the cursor is reset when the next stage is entered).
        let proceed = |e: &mut bevy_ecs::system::EntityCommands| {
            e.insert(InteractionPointCursor(npoints))
                .insert(ResolveTransition);
        };

        match out.decision {
            PointOutcome::Abort => {
                state.status = AgentStatus::Cancelled;
            }
            PointOutcome::Approve { user_text } => {
                state.status = AgentStatus::Active;
                inject(&mut window, &name, "", &user_text);
                let next = idx + 1;
                if next >= npoints {
                    proceed(&mut e); // all points satisfied ⇒ transition
                } else {
                    e.insert(InteractionPointCursor(next))
                        .insert(InteractionPointRounds(0))
                        .insert(ReadyForInteractionPoint);
                }
            }
            PointOutcome::Directive {
                user_text,
                directive,
            } => {
                state.status = AgentStatus::Active;
                inject(&mut window, &name, "", &user_text);
                if round + 1 >= MAX_REVISION_ROUNDS {
                    proceed(&mut e); // revision cap ⇒ proceed
                } else {
                    // Stay on this point; re-run inference in-stage on the directive.
                    inject(&mut window, &name, "directive: ", &directive);
                    e.insert(InteractionPointRounds(round + 1))
                        .insert(ReadyToInfer);
                }
            }
            PointOutcome::Edit { user_text, edited } => {
                state.status = AgentStatus::Active;
                inject(&mut window, &name, "", &user_text);
                if round + 1 >= MAX_REVISION_ROUNDS {
                    proceed(&mut e);
                } else {
                    if !edited.is_empty() {
                        let note = format!(
                            "edited the output directly. Adopt this exact text as the \
                             authoritative version and re-present it:\n{edited}"
                        );
                        inject(&mut window, &name, "", &note);
                        // Surface the adopted text in the stage output so observers
                        // (e.g. the dashboard's output pane, which reads output.log)
                        // reflect the revision rather than the pre-edit version.
                        if let Some(mut buf) = io_buf {
                            buf.output.push((
                                cursor.index,
                                format!("\n─── Updated (your edit) ───\n{edited}"),
                            ));
                        }
                    }
                    // Re-present the same point with the edit applied (no re-infer).
                    e.insert(InteractionPointRounds(round + 1))
                        .insert(ReadyForInteractionPoint);
                }
            }
        }
    }
}

/// Inject a `User [name] <prefix><text>` line into the conversation region (no-op
/// on empty text), so the agent sees the user's selection / directive / edit.
fn inject(window: &mut ContextWindow, name: &str, prefix: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let content = format!("User [{name}] {prefix}{text}");
    let tokens = content.len() / 4 + 1;
    let _ = window.add_to_region("conversation", content, tokens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::AgentStatus;
    use leviath_core::interaction::InteractionResponse;
    use leviath_core::{Region, RegionKind};
    use tokio::sync::mpsc::unbounded_channel;

    // ── builders ──

    fn point(name: &str, style: InteractionStyle, options: &[&str]) -> InteractionPoint {
        InteractionPoint {
            name: name.to_string(),
            prompt: "Choose".to_string(),
            required: true,
            style,
            options: options.iter().map(|s| s.to_string()).collect(),
            directives: HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
        }
    }

    /// The plan_approval point: approve / revise (directive) / edit / abort.
    fn plan_point() -> InteractionPoint {
        let mut p = point(
            "plan_approval",
            InteractionStyle::MultipleChoice,
            &["Approve", "Revise", "Add detail", "Abort"],
        );
        p.directives
            .insert("Revise".to_string(), "revise the plan".to_string());
        p.abort_options = vec!["Abort".to_string()];
        p.edit_options = vec!["Add detail".to_string()];
        p
    }

    fn blueprint_with(points: Vec<InteractionPoint>) -> AgentBlueprint {
        let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
        let mut stage = leviath_core::Stage::new(
            "plan".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        stage.mode = StageMode::InteractivePoints { points };
        let bp =
            leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        AgentBlueprint(bp)
    }

    /// A single-stage blueprint whose stage is *not* an interactive-points stage.
    fn noninteractive_bp() -> AgentBlueprint {
        let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
        let stage = leviath_core::Stage::new(
            "auto".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        AgentBlueprint(leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage],
            layout,
        ))
    }

    fn agent_state(status: AgentStatus) -> AgentState {
        AgentState {
            agent_id: "run-1".to_string(),
            current_stage: "plan".to_string(),
            iteration: 1,
            status,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w
    }

    fn infer(text: &str) -> InferenceResult {
        InferenceResult {
            response: text.to_string(),
            tool_calls: vec![],
            tokens_used: 0,
            timestamp: 0,
        }
    }

    // ── pure helpers ──

    #[test]
    fn normalize_folds_dashes_and_whitespace() {
        assert_eq!(
            normalize_for_followup("Revise \u{2014} now"),
            "Revise - now"
        );
        assert_eq!(normalize_for_followup("a\u{2013}b"), "a-b");
        assert_eq!(normalize_for_followup("  x   y  "), "x y");
    }

    #[test]
    fn option_matches_exact_normalized_and_miss() {
        let opts = vec!["Abort \u{2014} now".to_string()];
        assert!(option_matches(&opts, "Abort \u{2014} now")); // exact
        assert!(option_matches(&opts, "Abort - now")); // normalized
        assert!(!option_matches(&opts, "Approve")); // miss
    }

    #[test]
    fn lookup_directive_exact_normalized_and_none() {
        let mut d = HashMap::new();
        d.insert("Revise \u{2014} x".to_string(), "do it".to_string());
        assert_eq!(lookup_directive(&d, "Revise \u{2014} x"), Some("do it"));
        assert_eq!(lookup_directive(&d, "Revise - x"), Some("do it"));
        assert_eq!(lookup_directive(&d, "Approve"), None);
    }

    #[test]
    fn build_point_request_by_style() {
        use leviath_core::interaction::InteractionKind;
        let mc = build_point_request(
            &point("p", InteractionStyle::MultipleChoice, &["a", "b"]),
            "id".to_string(),
        );
        assert_eq!(mc.kind, InteractionKind::MultipleChoice);
        assert_eq!(mc.options.len(), 2);
        let cf = build_point_request(
            &point("p", InteractionStyle::Confirm, &[]),
            "id".to_string(),
        );
        assert_eq!(cf.kind, InteractionKind::Confirm);
        let ft = build_point_request(
            &point("p", InteractionStyle::FreeText, &[]),
            "id".to_string(),
        );
        assert_eq!(ft.kind, InteractionKind::FreeText);
    }

    #[test]
    fn resolve_answer_choice_index_fallback_and_value() {
        let opts = vec!["A".to_string(), "B".to_string()];
        let mut r = InteractionResponse::text("q", "");
        r.choice_index = Some(1);
        assert_eq!(resolve_answer(&r, &opts), "B"); // choice → option
        r.choice_index = Some(9); // out of range → fall to value
        r.value = Some("typed".to_string());
        assert_eq!(resolve_answer(&r, &opts), "typed");
        let empty = InteractionResponse::text("q", "");
        assert_eq!(resolve_answer(&empty, &opts), ""); // no choice, empty value
    }

    #[test]
    fn route_answer_covers_all_four() {
        let p = plan_point();
        assert_eq!(route_answer(&p, "Abort".to_string()), Routed::Abort);
        assert_eq!(
            route_answer(&p, "Add detail".to_string()),
            Routed::Edit {
                user_text: "Add detail".to_string()
            }
        );
        assert_eq!(
            route_answer(&p, "Revise".to_string()),
            Routed::Directive {
                user_text: "Revise".to_string(),
                directive: "revise the plan".to_string(),
            }
        );
        assert_eq!(
            route_answer(&p, "Approve".to_string()),
            Routed::Approve {
                user_text: "Approve".to_string()
            }
        );
    }

    #[test]
    fn inject_skips_empty_and_appends_nonempty() {
        let mut w = window();
        inject(&mut w, "plan", "", "");
        assert_eq!(w.get_region("conversation").unwrap().current_tokens, 0);
        inject(&mut w, "plan", "directive: ", "do x");
        assert!(w.get_region("conversation").unwrap().current_tokens > 0);
    }

    #[test]
    fn stage_points_some_for_interactive_none_otherwise() {
        let bp = blueprint_with(vec![plan_point()]);
        assert!(stage_points(&bp, &StageCursor { index: 0 }).is_some());
        // A non-interactive stage.
        let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
        let stage = leviath_core::Stage::new(
            "auto".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        let bp2 = AgentBlueprint(leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage],
            layout,
        ));
        assert!(stage_points(&bp2, &StageCursor { index: 0 }).is_none());
    }

    // ── gate ──

    fn run_gate(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(gate_interaction_points);
        s.run(world);
    }

    #[test]
    fn gate_intercepts_unsatisfied_interactive_stage() {
        let mut world = World::new();
        let e = world
            .spawn((
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                ResolveTransition,
            ))
            .id();
        run_gate(&mut world);
        assert!(world.get::<ReadyForInteractionPoint>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
    }

    #[test]
    fn gate_lets_satisfied_or_empty_or_noninteractive_proceed() {
        let mut world = World::new();
        // cursor past the (single) point ⇒ satisfied.
        let done = world
            .spawn((
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                InteractionPointCursor(1),
                ResolveTransition,
            ))
            .id();
        // empty points.
        let empty = world
            .spawn((
                blueprint_with(vec![]),
                StageCursor { index: 0 },
                ResolveTransition,
            ))
            .id();
        // non-interactive stage.
        let auto = world
            .spawn((
                noninteractive_bp(),
                StageCursor { index: 0 },
                ResolveTransition,
            ))
            .id();
        run_gate(&mut world);
        assert!(world.get::<ResolveTransition>(done).is_some());
        assert!(world.get::<ReadyForInteractionPoint>(done).is_none());
        assert!(world.get::<ResolveTransition>(empty).is_some());
        assert!(world.get::<ResolveTransition>(auto).is_some());
        assert!(world.get::<ReadyForInteractionPoint>(auto).is_none());
    }

    // ── dispatch ──

    #[tokio::test]
    async fn dispatch_noop_without_hub_or_stage() {
        let mut world = World::new();
        let e = world
            .spawn((
                agent_state(AgentStatus::Active),
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                infer("plan"),
                ReadyForInteractionPoint,
            ))
            .id();
        // No InteractionHub / InteractionPointStage resources ⇒ early return.
        let mut s = Schedule::default();
        s.add_systems(dispatch_interaction_point);
        s.run(&mut world);
        assert!(world.get::<ReadyForInteractionPoint>(e).is_some()); // untouched
    }

    fn dispatch_world() -> (World, InteractionHub) {
        let hub = InteractionHub::new();
        let (tx, _rx) = unbounded_channel();
        let mut world = World::new();
        world.insert_resource(hub.clone());
        world.insert_resource(InteractionPointStage {
            outcomes: tx,
            wake: Arc::new(Notify::new()),
            runtime: Handle::current(),
        });
        (world, hub)
    }

    #[tokio::test]
    async fn dispatch_skips_non_active_agent() {
        let (mut world, _hub) = dispatch_world();
        let e = world
            .spawn((
                agent_state(AgentStatus::Waiting),
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                infer("plan"),
                ReadyForInteractionPoint,
            ))
            .id();
        let mut s = Schedule::default();
        s.add_systems(dispatch_interaction_point);
        s.run(&mut world);
        assert!(world.get::<ReadyForInteractionPoint>(e).is_some()); // not dispatched
    }

    #[tokio::test]
    async fn dispatch_falls_through_when_point_missing() {
        let (mut world, _hub) = dispatch_world();
        // cursor past the single point ⇒ no point to ask ⇒ ResolveTransition.
        let e = world
            .spawn((
                agent_state(AgentStatus::Active),
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                InteractionPointCursor(5),
                infer("plan"),
                ReadyForInteractionPoint,
            ))
            .id();
        let mut s = Schedule::default();
        s.add_systems(dispatch_interaction_point);
        s.run(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert!(world.get::<ReadyForInteractionPoint>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_spawns_ask_and_awaits() {
        let (mut world, hub) = dispatch_world();
        let e = world
            .spawn((
                agent_state(AgentStatus::Active),
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                infer("the plan"),
                ReadyForInteractionPoint,
            ))
            .id();
        let mut s = Schedule::default();
        s.add_systems(dispatch_interaction_point);
        s.run(&mut world);
        assert!(world.get::<AwaitingInteractionPoint>(e).is_some());
        assert!(world.get::<ReadyForInteractionPoint>(e).is_none());
        // The ask task registered a request in the hub.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(hub.pending().len(), 1);
    }

    // ── collect ──

    fn collect_world() -> (
        World,
        tokio::sync::mpsc::UnboundedSender<InteractionPointOutcome>,
    ) {
        let (tx, rx) = unbounded_channel();
        let mut world = World::new();
        world.insert_resource(InteractionPointResults(rx));
        (world, tx)
    }

    fn run_collect(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(collect_interaction_point);
        s.run(world);
    }

    fn spawn_awaiting(world: &mut World, points: Vec<InteractionPoint>) -> Entity {
        world
            .spawn((
                agent_state(AgentStatus::Waiting),
                window(),
                blueprint_with(points),
                StageCursor { index: 0 },
                AwaitingInteractionPoint,
            ))
            .id()
    }

    #[test]
    fn collect_approve_single_point_proceeds() {
        let (mut world, tx) = collect_world();
        let e = spawn_awaiting(&mut world, vec![plan_point()]);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Approve {
                user_text: "Approve".to_string(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert_eq!(world.get::<InteractionPointCursor>(e).unwrap().0, 1);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(world.get::<AwaitingInteractionPoint>(e).is_none());
    }

    #[test]
    fn collect_approve_advances_to_next_point() {
        let (mut world, tx) = collect_world();
        let e = spawn_awaiting(
            &mut world,
            vec![
                point("first", InteractionStyle::Confirm, &[]),
                point("second", InteractionStyle::Confirm, &[]),
            ],
        );
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Approve {
                user_text: String::new(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert_eq!(world.get::<InteractionPointCursor>(e).unwrap().0, 1);
        assert!(world.get::<ReadyForInteractionPoint>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
    }

    #[test]
    fn collect_abort_cancels() {
        let (mut world, tx) = collect_world();
        let e = spawn_awaiting(&mut world, vec![plan_point()]);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Abort,
        })
        .unwrap();
        run_collect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Cancelled
        );
        assert!(world.get::<ResolveTransition>(e).is_none());
    }

    #[test]
    fn collect_directive_reinfers_then_caps() {
        let (mut world, tx) = collect_world();
        let e = spawn_awaiting(&mut world, vec![plan_point()]);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Directive {
                user_text: "Revise".to_string(),
                directive: "do it".to_string(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert_eq!(world.get::<InteractionPointRounds>(e).unwrap().0, 1);
        assert!(world.get::<ResolveTransition>(e).is_none());

        // At the cap, a further directive proceeds instead of re-inferring.
        world
            .entity_mut(e)
            .insert(InteractionPointRounds(MAX_REVISION_ROUNDS - 1))
            .insert(AwaitingInteractionPoint);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Directive {
                user_text: String::new(),
                directive: "again".to_string(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
    }

    #[test]
    fn collect_edit_surfaces_the_adopted_text_in_stage_output() {
        let (mut world, tx) = collect_world();
        let e = world
            .spawn((
                agent_state(AgentStatus::Waiting),
                window(),
                blueprint_with(vec![plan_point()]),
                StageCursor { index: 0 },
                AwaitingInteractionPoint,
                StageIoBuffer::default(),
            ))
            .id();
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Edit {
                user_text: "Add detail".to_string(),
                edited: "the revised plan".to_string(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        // The adopted text is buffered for stages/<idx>/output.log, tagged with
        // the current stage index, so observers reflect the revision.
        let buf = world.get::<StageIoBuffer>(e).unwrap();
        assert_eq!(buf.output.len(), 1);
        assert_eq!(buf.output[0].0, 0);
        assert!(buf.output[0].1.contains("the revised plan"));
    }

    #[test]
    fn collect_edit_represents_then_caps() {
        let (mut world, tx) = collect_world();
        let e = spawn_awaiting(&mut world, vec![plan_point()]);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Edit {
                user_text: "Add detail".to_string(),
                edited: "the edited plan".to_string(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ReadyForInteractionPoint>(e).is_some());
        assert_eq!(world.get::<InteractionPointRounds>(e).unwrap().0, 1);
        // The edited text was injected.
        let after_first = world.get::<ContextWindow>(e).unwrap().current_tokens;
        assert!(after_first > 0);

        // An empty edit re-presents too, but injects nothing new.
        world
            .entity_mut(e)
            .insert(InteractionPointRounds(0))
            .insert(AwaitingInteractionPoint);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Edit {
                user_text: String::new(),
                edited: String::new(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ReadyForInteractionPoint>(e).is_some());
        assert_eq!(
            world.get::<ContextWindow>(e).unwrap().current_tokens,
            after_first
        );

        // At the cap, an edit proceeds instead of re-presenting.
        world
            .entity_mut(e)
            .insert(InteractionPointRounds(MAX_REVISION_ROUNDS - 1))
            .insert(AwaitingInteractionPoint);
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Edit {
                user_text: String::new(),
                edited: String::new(), // empty edit ⇒ no injection branch
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
    }

    #[test]
    fn collect_on_noninteractive_stage_proceeds() {
        // An outcome for an agent whose stage isn't interactive (npoints = 0):
        // approve's next index immediately satisfies, so it proceeds.
        let (mut world, tx) = collect_world();
        let e = world
            .spawn((
                agent_state(AgentStatus::Waiting),
                window(),
                noninteractive_bp(),
                StageCursor { index: 0 },
                AwaitingInteractionPoint,
            ))
            .id();
        tx.send(InteractionPointOutcome {
            entity: e,
            decision: PointOutcome::Approve {
                user_text: String::new(),
            },
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
    }

    #[test]
    fn collect_drops_outcome_for_missing_agent() {
        let (mut world, tx) = collect_world();
        tx.send(InteractionPointOutcome {
            entity: Entity::from_raw(999),
            decision: PointOutcome::Abort,
        })
        .unwrap();
        run_collect(&mut world); // no panic
    }

    // ── the async ask task ──

    async fn drive_point(
        point: InteractionPoint,
        answer: impl FnOnce(&InteractionHub, String),
    ) -> PointOutcome {
        let hub = InteractionHub::new();
        let (tx, mut rx) = unbounded_channel();
        let task = {
            let hub = hub.clone();
            tokio::spawn(run_interaction_point(
                Entity::from_raw(1),
                hub,
                "run".to_string(),
                point,
                "body".to_string(),
                0,
                tx,
                Arc::new(Notify::new()),
            ))
        };
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let id = hub.pending()[0].1.id.clone();
        answer(&hub, id);
        task.await.unwrap();
        rx.recv().await.unwrap().decision
    }

    #[tokio::test]
    async fn run_point_approve() {
        let out = drive_point(plan_point(), |hub, id| {
            let mut r = InteractionResponse::text(&id, "");
            r.choice_index = Some(0); // Approve
            hub.answer(r);
        })
        .await;
        assert_eq!(
            out,
            PointOutcome::Approve {
                user_text: "Approve".to_string()
            }
        );
    }

    #[tokio::test]
    async fn run_point_abort_and_directive() {
        let abort = drive_point(plan_point(), |hub, id| {
            let mut r = InteractionResponse::text(&id, "");
            r.choice_index = Some(3); // Abort
            hub.answer(r);
        })
        .await;
        assert_eq!(abort, PointOutcome::Abort);

        let directive = drive_point(plan_point(), |hub, id| {
            let mut r = InteractionResponse::text(&id, "");
            r.choice_index = Some(1); // Revise
            hub.answer(r);
        })
        .await;
        assert_eq!(
            directive,
            PointOutcome::Directive {
                user_text: "Revise".to_string(),
                directive: "revise the plan".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn run_point_edit_does_second_ask() {
        // Selecting the edit option triggers a second (edit_text) ask; answer both.
        let hub = InteractionHub::new();
        let (tx, mut rx) = unbounded_channel();
        let task = {
            let hub = hub.clone();
            tokio::spawn(run_interaction_point(
                Entity::from_raw(1),
                hub,
                "run".to_string(),
                plan_point(),
                "body".to_string(),
                0,
                tx,
                Arc::new(Notify::new()),
            ))
        };
        // Answer the point with the edit option.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let id = hub.pending()[0].1.id.clone();
        let mut r = InteractionResponse::text(&id, "");
        r.choice_index = Some(2); // Add detail ⇒ edit
        hub.answer(r);
        // Then answer the edit request with the edited text.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let edit_id = hub.pending()[0].1.id.clone();
        hub.answer(InteractionResponse::text(&edit_id, "edited body"));
        task.await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap().decision,
            PointOutcome::Edit {
                user_text: "Add detail".to_string(),
                edited: "edited body".to_string(),
            }
        );
    }
}
