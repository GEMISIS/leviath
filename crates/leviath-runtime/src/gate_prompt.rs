//! Interactive taint-gate prompt lane.
//!
//! When the taint gate blocks an outbound tool call (it would leak
//! over-cleared data), the default is to return a `[blocked]` result. When an
//! [`InteractionHub`] is available (the daemon), the block instead becomes a
//! prompt — "Allow once / Allow for this session / Deny" — mirroring tool
//! approval. The user's choice is applied via
//! [`TaintGate::apply_resolution`](crate::taint::TaintGate::apply_resolution):
//! allow (once) executes the call; allow-for-session also raises the tool's
//! clearance so it isn't asked again; deny returns the `[blocked]` result.
//!
//! Flow (mirrors the interaction-point lane): [`dispatch_tools`](crate::pipeline::dispatch_tools)
//! holds the whole batch when a call is blocked-and-unresolved, spawning a
//! [`run_gate_prompt`] task per blocked call and marking the agent
//! [`AwaitingGatePrompt`]. [`collect_gate_prompt`] applies each resolution into
//! [`GateResolved`] and, once all are in, re-arms `ReadyForTools`; the re-run of
//! `dispatch_tools` then executes the approved calls and `[blocked]`s the denied
//! ones without re-prompting.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::TaintLevel;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::dynamic_interaction::InteractionBackend;
use crate::interaction_hub::InteractionHub;
use crate::taint::GateResolution;

/// Marks an agent holding its tool batch while `n` gate prompts are outstanding.
#[derive(Component, Debug, Clone, Copy)]
pub struct AwaitingGatePrompt(pub usize);

/// Per-agent record of resolved blocked calls, consumed by the tool-dispatch
/// re-run: `approved` call ids execute, `denied` call ids get their stored
/// `[blocked]` message. Removed once the batch is dispatched.
#[derive(Component, Debug, Clone, Default)]
pub struct GateResolved {
    /// Tool-call ids the user allowed (execute without re-checking the gate).
    pub approved: HashSet<String>,
    /// Tool-call ids the user denied, mapped to their block message.
    pub denied: HashMap<String, String>,
}

/// One resolved gate prompt, reported on the lane.
pub struct GatePromptOutcome {
    /// The agent the call belongs to.
    pub entity: Entity,
    /// The blocked tool call's id.
    pub tool_id: String,
    /// The tool name (for the audit event).
    pub tool_name: String,
    /// The taint level that caused the block.
    pub taint: TaintLevel,
    /// The tool's clearance level.
    pub clearance: TaintLevel,
    /// The user's resolution.
    pub resolution: GateResolution,
}

/// The sending side of the gate-prompt lane + the handle/wake to drive the ask.
#[derive(Resource)]
pub struct GatePromptStage {
    /// Where resolved outcomes are reported.
    pub outcomes: UnboundedSender<GatePromptOutcome>,
    /// Wakes the tick loop when an outcome lands.
    pub wake: Arc<Notify>,
    /// Runtime the ask task is spawned onto.
    pub runtime: Handle,
}

/// The receiving side of the gate-prompt lane, for the collect system.
#[derive(Resource)]
pub struct GatePromptResults(pub UnboundedReceiver<GatePromptOutcome>);

/// Map a multiple-choice answer to a resolution (default: `Deny`, per the
/// gate's safe default when there is no answer).
fn resolution_from_answer(resp: &InteractionResponse) -> GateResolution {
    match resp.choice_index {
        Some(0) => GateResolution::AllowOnce,
        Some(1) => GateResolution::AlwaysAllow,
        _ => GateResolution::Deny,
    }
}

/// Build the "how to resolve this blocked call?" prompt.
fn build_gate_request(
    id: String,
    tool_name: &str,
    taint: TaintLevel,
    clearance: TaintLevel,
) -> InteractionRequest {
    InteractionRequest::multiple_choice(
        id,
        format!(
            "Tool '{tool_name}' would send {taint:?}-level data over a channel cleared only for \
             {clearance:?}. Allow it?"
        ),
        vec![
            "Allow once".to_string(),
            "Allow for this session".to_string(),
            "Deny".to_string(),
        ],
        "taint_gate",
    )
}

/// Ask the user how to resolve a blocked outbound call, then report the
/// resolution on the lane and wake the tick loop.
#[allow(clippy::too_many_arguments)]
pub async fn run_gate_prompt(
    entity: Entity,
    hub: InteractionHub,
    agent_id: String,
    tool_id: String,
    tool_name: String,
    taint: TaintLevel,
    clearance: TaintLevel,
    outcomes: UnboundedSender<GatePromptOutcome>,
    wake: Arc<Notify>,
) {
    let backend = hub.backend_for(agent_id);
    let req = build_gate_request(format!("gate-{tool_id}"), &tool_name, taint, clearance);
    let resolution = resolution_from_answer(&backend.ask(req).await);
    let _ = outcomes.send(GatePromptOutcome {
        entity,
        tool_id,
        tool_name,
        taint,
        clearance,
        resolution,
    });
    wake.notify_one();
}

/// Collect: apply each resolved gate prompt into the agent's [`GateResolved`],
/// and once every outstanding prompt is in, re-arm the tool dispatch.
pub fn collect_gate_prompt(
    mut results: ResMut<GatePromptResults>,
    mut agents: Query<(
        &crate::components::AgentState,
        &mut AwaitingGatePrompt,
        &mut GateResolved,
        Option<&mut crate::taint::TaintGate>,
    )>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(out) = results.0.try_recv() {
        let Ok((state, mut awaiting, mut resolved, gate)) = agents.get_mut(out.entity) else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(out.entity);
        // Apply the resolution through the gate (raises clearance for AlwaysAllow).
        let denied = gate.and_then(|mut g| {
            g.apply_resolution(
                &state.agent_id,
                &out.tool_name,
                &out.tool_id,
                out.taint,
                out.clearance,
                out.resolution,
            )
        });
        match denied {
            Some((id, msg)) => {
                resolved.denied.insert(id, msg);
            }
            None => {
                resolved.approved.insert(out.tool_id.clone());
            }
        }
        awaiting.0 = awaiting.0.saturating_sub(1);
        if awaiting.0 == 0 {
            commands
                .entity(out.entity)
                .remove::<AwaitingGatePrompt>()
                .insert(crate::pipeline::ReadyForTools);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::AgentState;
    use crate::pipeline::ReadyForTools;
    use crate::taint::TaintGate;
    use leviath_core::SecurityConfig;
    use leviath_core::interaction::InteractionKind;
    use tokio::sync::mpsc::unbounded_channel;

    fn resp(choice: Option<usize>) -> InteractionResponse {
        let mut r = InteractionResponse::text("q", "");
        r.choice_index = choice;
        r
    }

    #[test]
    fn resolution_from_answer_maps_choices_and_defaults_deny() {
        assert_eq!(
            resolution_from_answer(&resp(Some(0))),
            GateResolution::AllowOnce
        );
        assert_eq!(
            resolution_from_answer(&resp(Some(1))),
            GateResolution::AlwaysAllow
        );
        assert_eq!(resolution_from_answer(&resp(Some(2))), GateResolution::Deny);
        assert_eq!(resolution_from_answer(&resp(None)), GateResolution::Deny);
    }

    #[test]
    fn build_gate_request_is_a_three_way_choice() {
        let req = build_gate_request(
            "id".to_string(),
            "shell",
            TaintLevel::Internal,
            TaintLevel::Public,
        );
        assert_eq!(req.kind, InteractionKind::MultipleChoice);
        assert_eq!(req.options.len(), 3);
        assert!(req.prompt.contains("shell"));
    }

    #[tokio::test]
    async fn run_gate_prompt_reports_the_resolution() {
        let hub = InteractionHub::new();
        let (tx, mut rx) = unbounded_channel();
        let task = {
            let hub = hub.clone();
            tokio::spawn(run_gate_prompt(
                Entity::from_raw(1),
                hub,
                "run".to_string(),
                "c1".to_string(),
                "shell".to_string(),
                TaintLevel::Internal,
                TaintLevel::Public,
                tx,
                Arc::new(Notify::new()),
            ))
        };
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let id = hub.pending()[0].1.id.clone();
        hub.answer({
            let mut r = InteractionResponse::text(&id, "");
            r.choice_index = Some(1); // Allow for session
            r
        });
        task.await.unwrap();
        let out = rx.recv().await.unwrap();
        assert_eq!(out.tool_id, "c1");
        assert_eq!(out.resolution, GateResolution::AlwaysAllow);
    }

    fn collect_world() -> (World, UnboundedSender<GatePromptOutcome>) {
        let (tx, rx) = unbounded_channel();
        let mut world = World::new();
        world.insert_resource(GatePromptResults(rx));
        (world, tx)
    }

    fn state() -> AgentState {
        AgentState {
            agent_id: "run".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status: crate::components::AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn gate() -> TaintGate {
        TaintGate::new(SecurityConfig {
            taint_tracking: true,
        })
    }

    fn run_collect(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(collect_gate_prompt);
        s.run(world);
    }

    #[test]
    fn collect_records_approval_and_rearms_when_last_resolves() {
        let (mut world, tx) = collect_world();
        let e = world
            .spawn((
                state(),
                AwaitingGatePrompt(2),
                GateResolved::default(),
                gate(),
            ))
            .id();
        // First outcome: allow-once ⇒ approved, still one outstanding.
        tx.send(GatePromptOutcome {
            entity: e,
            tool_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            taint: TaintLevel::Internal,
            clearance: TaintLevel::Public,
            resolution: GateResolution::AllowOnce,
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<AwaitingGatePrompt>(e).is_some());
        assert!(
            world
                .get::<GateResolved>(e)
                .unwrap()
                .approved
                .contains("c1")
        );
        assert!(world.get::<ReadyForTools>(e).is_none());

        // Second outcome: deny ⇒ denied map + count hits zero ⇒ re-armed.
        tx.send(GatePromptOutcome {
            entity: e,
            tool_id: "c2".to_string(),
            tool_name: "curl".to_string(),
            taint: TaintLevel::Internal,
            clearance: TaintLevel::Public,
            resolution: GateResolution::Deny,
        })
        .unwrap();
        run_collect(&mut world);
        assert!(world.get::<AwaitingGatePrompt>(e).is_none());
        assert!(
            world
                .get::<GateResolved>(e)
                .unwrap()
                .denied
                .contains_key("c2")
        );
        assert!(world.get::<ReadyForTools>(e).is_some());
    }

    #[test]
    fn collect_drops_outcome_for_missing_agent() {
        let (mut world, tx) = collect_world();
        tx.send(GatePromptOutcome {
            entity: Entity::from_raw(999),
            tool_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            taint: TaintLevel::Internal,
            clearance: TaintLevel::Public,
            resolution: GateResolution::Deny,
        })
        .unwrap();
        run_collect(&mut world); // no panic
    }
}
