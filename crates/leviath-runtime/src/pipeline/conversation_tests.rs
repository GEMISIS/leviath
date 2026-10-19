//! Tests for [`super`]: the pieces of an interactive stage's wait, each on a
//! bare world. The whole loop through a host is in `host_conversation_tests`.

use super::*;
use crate::spec::graph::{InteractiveDef, StageMode};
use crate::test_graph as g;

/// A one-stage graph whose stage runs in `mode`.
fn graph(mode: StageMode) -> crate::spec::graph::RunGraph {
    let layout = g::layout(
        vec![g::region(
            "conversation",
            leviath_core::RegionKind::Clearable,
            10_000,
        )],
        12_000,
    );
    let stage = crate::spec::graph::StageDef {
        mode,
        ..g::stage("s")
    };
    g::graph(vec![stage], layout)
}

fn interactive(idle: Option<u64>) -> StageMode {
    StageMode::Interactive(InteractiveDef {
        idle_timeout_secs: idle,
    })
}

fn agent(status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: "r".to_string(),
        current_stage: "s".to_string(),
        current_visit: String::new(),
        iteration: 0,
        status,
        spawned_children_ids: Vec::new(),
        pending_wait: None,
        accepts_messages: true,
    }
}

/// A run in a stage of `mode`, with `status`, parked or not.
fn run(world: &mut World, mode: StageMode, status: AgentStatus, parked: bool) -> Entity {
    let mut entity = world.spawn((
        g::both(graph(mode)),
        StageCursor { index: 0 },
        agent(status),
        StageProgress::default(),
        StageIoBuffer::default(),
    ));
    if parked {
        entity.insert(AwaitingMessage);
    }
    entity.id()
}

#[test]
fn only_an_interactive_stage_whose_conversation_is_open_parks() {
    let open = crate::spec::graph::StageDef {
        mode: interactive(None),
        ..g::stage("s")
    };
    assert!(parks_after_reply(Some(&open), false));
    assert!(!parks_after_reply(Some(&open), true));
    assert!(!parks_after_reply(Some(&g::stage("s")), false));
    assert!(!parks_after_reply(None, false));
}

#[test]
fn the_idle_limit_is_the_stages_own_else_the_unattended_default() {
    let limited = crate::spec::graph::StageDef {
        mode: interactive(Some(30)),
        ..g::stage("s")
    };
    let open = crate::spec::graph::StageDef {
        mode: interactive(None),
        ..g::stage("s")
    };
    assert_eq!(idle_limit(&limited, false), Some(30));
    assert_eq!(idle_limit(&limited, true), Some(30));
    assert_eq!(idle_limit(&open, false), None);
    assert_eq!(idle_limit(&open, true), Some(DEFAULT_UNATTENDED_IDLE_SECS));
}

#[test]
fn end_conversation_closes_only_an_interactive_stage() {
    let open = crate::spec::graph::StageDef {
        mode: interactive(None),
        ..g::stage("s")
    };
    let (text, closed) = end_conversation_call(Some(&open));
    assert!(closed);
    assert!(text.contains("moves on"));
    let (text, closed) = end_conversation_call(Some(&g::stage("s")));
    assert!(!closed);
    assert!(text.starts_with("[error]"));
    assert!(!end_conversation_call(None).1);
}

/// Parking waits for the next message; a paused run stays paused, and the
/// wait started first is the one kept.
#[test]
fn parking_waits_and_keeps_a_pause() {
    let mut world = World::new();
    let active = world.spawn(ReadyForTransition).id();
    let paused = world.spawn(ReadyForTransition).id();
    let asked = world.spawn(ReadyForTransition).id();
    let mut working = agent(AgentStatus::Active);
    let mut held = agent(AgentStatus::Paused);
    let mut progress = StageProgress::default();
    let mut queue = bevy_ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, &world);
    park(
        &mut commands,
        active,
        false,
        Some(&mut working),
        &mut progress,
        100,
    );
    park(
        &mut commands,
        paused,
        false,
        Some(&mut held),
        &mut progress,
        200,
    );
    park(&mut commands, paused, false, None, &mut progress, 300);
    park(&mut commands, asked, true, None, &mut progress, 400);
    queue.apply(&mut world);

    assert_eq!(working.status, AgentStatus::Waiting);
    assert_eq!(held.status, AgentStatus::Paused);
    assert_eq!(progress.waiting_since, Some(100));
    for entity in [active, paused] {
        assert!(world.get::<AwaitingMessage>(entity).is_some());
        assert!(world.get::<ReadyForTransition>(entity).is_none());
    }
    // One with a message it has not answered asks again instead of waiting.
    assert!(world.get::<ReadyToInfer>(asked).is_some());
    assert!(world.get::<AwaitingMessage>(asked).is_none());
}

/// A message wakes the run to ask its model again, and the wait is taken off
/// the stage's clock. A paused run is woken but stays paused.
#[test]
fn a_message_wakes_the_run_and_credits_the_wait() {
    let mut world = World::new();
    let entity = world.spawn(AwaitingMessage).id();
    let mut waiting = agent(AgentStatus::Waiting);
    let mut paused = agent(AgentStatus::Paused);
    let mut progress = StageProgress {
        stage_started_at: Some(10),
        waiting_since: Some(50),
        ..StageProgress::default()
    };
    let mut queue = bevy_ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, &world);
    wake(&mut commands, entity, &mut waiting, Some(&mut progress), 80);
    wake(&mut commands, entity, &mut paused, None, 90);
    queue.apply(&mut world);

    assert_eq!(waiting.status, AgentStatus::Active);
    assert_eq!(paused.status, AgentStatus::Paused);
    assert_eq!(progress.waiting_since, None);
    assert_eq!(progress.stage_started_at, Some(40));
    assert!(world.get::<AwaitingMessage>(entity).is_none());
    assert!(world.get::<ReadyToInfer>(entity).is_some());
}

/// A parked stage whose limit has run out moves on through its transition;
/// one inside its limit, one with no limit and one somebody paused keep
/// waiting.
#[test]
fn an_idle_limit_that_runs_out_moves_the_stage_on() {
    let mut world = World::new();
    let now = chrono::Utc::now().timestamp();
    let expired = run(
        &mut world,
        interactive(Some(60)),
        AgentStatus::Waiting,
        true,
    );
    let fresh = run(
        &mut world,
        interactive(Some(600)),
        AgentStatus::Waiting,
        true,
    );
    let open = run(&mut world, interactive(None), AgentStatus::Waiting, true);
    let paused = run(&mut world, interactive(Some(60)), AgentStatus::Paused, true);
    let unstamped = run(
        &mut world,
        interactive(Some(60)),
        AgentStatus::Waiting,
        true,
    );
    let unlogged = run(
        &mut world,
        interactive(Some(60)),
        AgentStatus::Waiting,
        true,
    );
    world.entity_mut(unlogged).remove::<StageIoBuffer>();
    for entity in [expired, fresh, open, paused, unlogged] {
        world
            .get_mut::<StageProgress>(entity)
            .unwrap()
            .waiting_since = Some(now - 61);
    }
    world.run_system_cached(expire_idle_conversations).unwrap();
    assert!(world.get::<ResolveTransition>(unlogged).is_some());

    assert!(world.get::<ResolveTransition>(expired).is_some());
    assert!(world.get::<ConversationEnded>(expired).is_some());
    assert!(world.get::<AwaitingMessage>(expired).is_none());
    assert_eq!(
        world.get::<AgentState>(expired).unwrap().status,
        AgentStatus::Active
    );
    let logs = &world.get::<StageIoBuffer>(expired).unwrap().logs;
    assert!(logs[0].1.contains("moves on"));
    for entity in [fresh, open, paused, unstamped] {
        assert!(world.get::<AwaitingMessage>(entity).is_some());
        assert!(world.get::<ResolveTransition>(entity).is_none());
    }
    // A wait with no start recorded starts now, rather than counting from
    // nothing.
    assert!(
        world
            .get::<StageProgress>(unstamped)
            .unwrap()
            .waiting_since
            .is_some()
    );
}

/// A stage of a run nobody is watching has a limit even when it sets none.
#[test]
fn an_unattended_run_does_not_wait_for_ever() {
    let mut world = World::new();
    let now = chrono::Utc::now().timestamp();
    let entity = run(&mut world, interactive(None), AgentStatus::Waiting, true);
    let mut spec = (*world.get::<RunSpecC>(entity).unwrap().0).clone();
    spec.auto_answers.questions = true;
    world
        .entity_mut(entity)
        .insert(RunSpecC(std::sync::Arc::new(spec)));
    let since = now - i64::try_from(DEFAULT_UNATTENDED_IDLE_SECS).unwrap() - 1;
    world
        .get_mut::<StageProgress>(entity)
        .unwrap()
        .waiting_since = Some(since);
    world.run_system_cached(expire_idle_conversations).unwrap();
    assert!(world.get::<ResolveTransition>(entity).is_some());
}

/// A child run (a sub-agent, a fan-out worker) is talked to by the run that
/// started it, not a person, so it has the same limit when its stage sets
/// none, and does not hold its parent for ever.
#[test]
fn a_child_run_does_not_wait_for_ever() {
    let mut world = World::new();
    let now = chrono::Utc::now().timestamp();
    let entity = run(&mut world, interactive(None), AgentStatus::Waiting, true);
    let mut spec = (*world.get::<RunSpecC>(entity).unwrap().0).clone();
    spec.placement.parent = Some(crate::spec::names::RunId::new("parent").unwrap());
    world
        .entity_mut(entity)
        .insert(RunSpecC(std::sync::Arc::new(spec)));
    let since = now - i64::try_from(DEFAULT_UNATTENDED_IDLE_SECS).unwrap() - 1;
    world
        .get_mut::<StageProgress>(entity)
        .unwrap()
        .waiting_since = Some(since);
    world.run_system_cached(expire_idle_conversations).unwrap();
    assert!(world.get::<ResolveTransition>(entity).is_some());
}

/// The mark the end of a conversation leaves is dropped once the run is in
/// its next stage.
#[test]
fn entering_a_stage_forgets_the_ended_conversation() {
    let mut world = World::new();
    let entered = world
        .spawn((
            ConversationEnded,
            StageJustEntered {
                index: 1,
                name: "next".to_string(),
            },
        ))
        .id();
    let staying = world.spawn(ConversationEnded).id();
    world.run_system_cached(forget_ended_conversations).unwrap();
    assert!(world.get::<ConversationEnded>(entered).is_none());
    assert!(world.get::<ConversationEnded>(staying).is_some());
}

/// A person ending the conversation: a parked stage moves on now, one
/// mid-turn after its reply.
#[test]
fn a_person_ends_a_parked_or_a_busy_conversation() {
    let mut world = World::new();
    let parked = run(&mut world, interactive(None), AgentStatus::Waiting, true);
    world
        .get_mut::<StageProgress>(parked)
        .unwrap()
        .waiting_since = Some(1);
    assert_eq!(
        end_conversation(&mut world, parked),
        Ok(EndConversation::Ended)
    );
    assert!(world.get::<ResolveTransition>(parked).is_some());
    assert!(world.get::<ConversationEnded>(parked).is_some());
    assert!(world.get::<AwaitingMessage>(parked).is_none());
    assert_eq!(
        world.get::<AgentState>(parked).unwrap().status,
        AgentStatus::Active
    );
    assert_eq!(
        world.get::<StageProgress>(parked).unwrap().waiting_since,
        None
    );

    let busy = run(&mut world, interactive(None), AgentStatus::Active, false);
    assert_eq!(
        end_conversation(&mut world, busy),
        Ok(EndConversation::AfterReply)
    );
    assert!(world.get::<ConversationEnded>(busy).is_some());
    assert!(world.get::<ResolveTransition>(busy).is_none());

    // A paused parked run stays paused; it moves on once resumed.
    let paused = run(&mut world, interactive(None), AgentStatus::Paused, true);
    world.entity_mut(paused).remove::<StageProgress>();
    assert_eq!(
        end_conversation(&mut world, paused),
        Ok(EndConversation::Ended)
    );
    assert_eq!(
        world.get::<AgentState>(paused).unwrap().status,
        AgentStatus::Paused
    );
}

/// There is no conversation to end in a stage that is not interactive, or in
/// a run that has ended.
#[test]
fn ending_a_conversation_that_is_not_there_is_refused() {
    let mut world = World::new();
    let autonomous = run(
        &mut world,
        StageMode::Autonomous,
        AgentStatus::Active,
        false,
    );
    let refused = end_conversation(&mut world, autonomous).unwrap_err();
    assert!(refused.contains("not interactive"));
    let over = run(&mut world, interactive(None), AgentStatus::Complete, true);
    let refused = end_conversation(&mut world, over).unwrap_err();
    assert!(refused.contains("already ended"));
    let bare = world.spawn(agent(AgentStatus::Active)).id();
    let refused = end_conversation(&mut world, bare).unwrap_err();
    assert!(refused.contains("not interactive"));
}
