//! Inbound message routing and inbox delivery.

use super::*;

/// The receiving end of the world's inbound-message channel. Clients (the
/// control API) send `AgentMessage`s here; the delivery system routes and
/// delivers them.
#[derive(Resource)]
pub struct MessageIntake(pub UnboundedReceiver<AgentMessage>);

/// Message-delivery system: route inbound messages to their target agents'
/// inboxes (by agent id), then deliver each inbox into the agent's context
/// window - but only for agents whose current stage accepts messages; otherwise
/// the messages wait in the inbox for a stage that does. Ported from
/// `AgentEngine::process_messages` / `deliver_inbox_messages`.
pub fn deliver_messages(
    mut intake: ResMut<MessageIntake>,
    mut agents: Query<(Entity, &AgentState, &mut MessageInbox, &mut ContextWindow)>,
) {
    crate::tick_scope::clear();
    // Route inbound channel messages to their target agent's inbox.
    let mut incoming = Vec::new();
    while let Ok(msg) = intake.0.try_recv() {
        incoming.push(msg);
    }
    for msg in incoming {
        for (entity, state, mut inbox, _) in agents.iter_mut() {
            crate::tick_scope::enter(entity);
            if state.agent_id == msg.agent_id {
                inbox.push(msg.clone());
                break;
            }
        }
        // Unmatched target ⇒ dropped (agent no longer exists).
    }

    // Deliver inboxes into context windows for agents that accept messages.
    for (entity, state, mut inbox, mut window) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if !state.accepts_messages {
            continue; // hold until a stage that accepts messages
        }
        for msg in inbox.drain_all() {
            let region = msg.target_region.as_deref().unwrap_or("conversation");
            let tokens = leviath_core::estimate_tokens(&msg.content);
            let _ = window.add_typed_entry(
                region,
                leviath_core::EntryKind::UserMessage,
                msg.content.clone(),
                tokens,
            );
        }
    }
}
