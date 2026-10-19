//! Interactive stages: after a reply with no tool calls the stage waits for
//! the next message instead of moving on.
//!
//! A parked stage holds [`AwaitingMessage`] and is `Waiting`. It holds no
//! inference or tool slot, since nothing is in flight, and it is written to the
//! run file as parked, so a daemon restart puts it back waiting without a model
//! call. Three things move it:
//!
//! - A message: delivered to the conversation like any mid-run message, and
//!   the stage asks its model again ([`wake`]).
//! - The end of the conversation: the model calls `end_conversation`, or a
//!   person ends it from outside. The stage then leaves through its ordinary
//!   transition, edges and all ([`ConversationEnded`]).
//! - Its idle limit running out ([`expire_idle_conversations`]). A run nobody
//!   is watching has one even when the stage sets none, so it never waits for a
//!   person for ever.

use super::*;
use crate::insert::RunSpecC;

/// An interactive stage replied and waits for the next message.
///
/// A phase marker like the others: the message-delivery system and the idle
/// limit are the systems that claim an agent resting on it.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct AwaitingMessage;

/// The conversation in the current interactive stage is over: its next
/// accepted reply moves the stage on rather than parking it.
///
/// Set by the `end_conversation` tool, by a person ending the conversation
/// and by the idle limit, and dropped as the next stage is entered. Kept for
/// the rest of the visit, so a stage a transition gate sends back to finish
/// something leaves again once it has, rather than waiting for another
/// message.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct ConversationEnded;

/// The idle limit of an interactive stage in a run nobody is watching (one
/// whose own questions are answered for it), when the stage sets none.
pub const DEFAULT_UNATTENDED_IDLE_SECS: u64 = 120;

/// Whether an interactive stage's accepted reply parks it: the stage is
/// interactive and its conversation has not been ended.
pub(crate) fn parks_after_reply(stage: Option<&crate::spec::graph::StageDef>, ended: bool) -> bool {
    !ended && stage.is_some_and(|s| s.mode.interactive().is_some())
}

/// Park the agent on `entity`: it waits for the next message. A paused agent
/// stays paused, and comes back waiting when it is resumed.
pub(crate) fn park(
    commands: &mut Commands,
    entity: Entity,
    state: Option<&mut AgentState>,
    progress: &mut StageProgress,
    now: i64,
) {
    commands
        .entity(entity)
        .remove::<ReadyForTransition>()
        .insert(AwaitingMessage);
    if let Some(state) = state.filter(|s| s.status == AgentStatus::Active) {
        state.status = AgentStatus::Waiting;
    }
    progress.waiting_since.get_or_insert(now);
}

/// A message reached the parked agent on `entity`: it asks its model again,
/// in the same stage, with the message in its conversation. A paused agent
/// stays paused and asks once it is resumed.
pub(crate) fn wake(
    commands: &mut Commands,
    entity: Entity,
    state: &mut AgentState,
    progress: Option<&mut StageProgress>,
    now: i64,
) {
    commands
        .entity(entity)
        .remove::<AwaitingMessage>()
        .insert(ReadyToInfer);
    if state.status == AgentStatus::Waiting {
        state.status = AgentStatus::Active;
    }
    if let Some(progress) = progress {
        super::persist::credit_wait_to_stage_clock(progress, now);
    }
}

/// How long the stage waits for a message before it moves on, or `None` to
/// wait for as long as it takes: the stage's own limit, else the unattended
/// default for a run whose questions are answered for it.
pub(crate) fn idle_limit(stage: &crate::spec::graph::StageDef, unattended: bool) -> Option<u64> {
    stage
        .mode
        .interactive()
        .and_then(|def| def.idle_timeout_secs)
        .or(unattended.then_some(DEFAULT_UNATTENDED_IDLE_SECS))
}

/// Answer an `end_conversation` call made in `stage`: the text the model
/// reads, and whether the conversation is now closed. Only an interactive
/// stage has one to close.
pub(crate) fn end_conversation_call(
    stage: Option<&crate::spec::graph::StageDef>,
) -> (String, bool) {
    let tool = leviath_core::stage_tools::END_CONVERSATION_TOOL;
    match stage.and_then(|s| s.mode.interactive()) {
        Some(_) => (
            "The conversation is closed. The stage moves on once this turn is done.".to_string(),
            true,
        ),
        None => (
            format!(
                "[error] {tool} closes an interactive stage's conversation, and this stage is \
                 not interactive"
            ),
            false,
        ),
    }
}

/// What `expire_idle_conversations` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type IdleQuery = (
    Entity,
    &'static mut AgentState,
    &'static RunSpecC,
    &'static StageCursor,
    &'static mut StageProgress,
    Option<&'static mut StageIoBuffer>,
);

/// Move on every parked stage whose idle limit has run out, as though the
/// conversation had been ended.
///
/// Only a `Waiting` agent's clock is read: a paused or cancelled one is held
/// by somebody, and its limit waits with it. The world is driven at least
/// every thirty seconds, so a limit is acted on within that of running out.
pub(crate) fn expire_idle_conversations(
    mut agents: Query<IdleQuery, With<AwaitingMessage>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let now = chrono::Utc::now().timestamp();
    for (entity, mut state, spec, cursor, mut progress, buffer) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Waiting {
            continue;
        }
        let stage = &spec.0.graph.stages[cursor.index];
        let Some(limit) = idle_limit(stage, spec.0.auto_answers.questions) else {
            continue;
        };
        let since = *progress.waiting_since.get_or_insert(now);
        let waited = now.saturating_sub(since);
        if waited < i64::try_from(limit).unwrap_or(i64::MAX) {
            continue;
        }
        let line = format!(
            "[conversation] no message for {waited}s (the idle limit is {limit}s), so stage '{}' moves on",
            stage.name
        );
        if let Some(mut buffer) = buffer {
            buffer.logs.push((cursor.index, line));
        }
        super::persist::credit_wait_to_stage_clock(&mut progress, now);
        state.status = AgentStatus::Active;
        commands
            .entity(entity)
            .remove::<AwaitingMessage>()
            .insert((ConversationEnded, ResolveTransition));
    }
}

/// Drop the ended mark from an agent that has just entered a stage: the
/// conversation it closed belonged to the stage it left.
pub(crate) fn forget_ended_conversations(
    agents: Query<Entity, (With<StageJustEntered>, With<ConversationEnded>)>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for entity in agents.iter() {
        crate::tick_scope::enter(entity);
        commands.entity(entity).remove::<ConversationEnded>();
    }
}

/// What ending the conversation on `entity` did, for a person who asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndConversation {
    /// The stage was waiting for a message and moves on now.
    Ended,
    /// The stage is mid-turn: it moves on once its model has replied.
    AfterReply,
}

/// End the conversation of the interactive stage the agent on `entity` is
/// in, for a person: a parked stage moves on now, through its ordinary
/// transition, and one mid-turn moves on after its reply rather than waiting
/// for another message. `Err` says why there is no conversation to end.
pub(crate) fn end_conversation(
    world: &mut World,
    entity: Entity,
) -> Result<EndConversation, String> {
    let mut agent = world.entity_mut(entity);
    let interactive = agent
        .get::<RunSpecC>()
        .zip(agent.get::<StageCursor>())
        .is_some_and(|(spec, cursor)| {
            spec.0.graph.stages[cursor.index]
                .mode
                .interactive()
                .is_some()
        });
    let parked = agent.contains::<AwaitingMessage>();
    let mut state = agent
        .get_mut::<AgentState>()
        .expect("a run in the world has its state");
    if is_terminal_status(&state.status) {
        return Err(format!(
            "run '{}' has already ended, so it has no conversation to end",
            state.agent_id
        ));
    }
    if !interactive {
        return Err(format!(
            "run '{}' is in stage '{}', which is not interactive, so it has no conversation to end",
            state.agent_id, state.current_stage
        ));
    }
    if !parked {
        agent.insert(ConversationEnded);
        return Ok(EndConversation::AfterReply);
    }
    if state.status == AgentStatus::Waiting {
        state.status = AgentStatus::Active;
    }
    let now = chrono::Utc::now().timestamp();
    if let Some(mut progress) = agent.get_mut::<StageProgress>() {
        super::persist::credit_wait_to_stage_clock(&mut progress, now);
    }
    agent
        .remove::<AwaitingMessage>()
        .insert((ConversationEnded, ResolveTransition));
    Ok(EndConversation::Ended)
}

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;
