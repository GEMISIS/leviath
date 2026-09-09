//! Inbound message routing and inbox delivery.

use super::*;

/// The receiving end of the world's inbound-message channel. Clients (the
/// control API) send `AgentMessage`s here; the delivery system routes and
/// delivers them.
#[derive(Resource)]
pub(crate) struct MessageIntake(pub UnboundedReceiver<AgentMessage>);

/// Message-delivery system: route inbound messages to their target agents'
/// inboxes (by agent id), then deliver each inbox into the agent's context
/// window - but only for agents whose current stage accepts messages; otherwise
/// the messages wait in the inbox for a stage that does. Ported from
/// `AgentEngine::process_messages` / `deliver_inbox_messages`.
pub(crate) fn deliver_messages(
    mut intake: ResMut<MessageIntake>,
    mut agents: Query<(Entity, &AgentState, &mut MessageInbox, &mut ContextWindow)>,
    media: crate::blob_store::MediaParams,
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
            let region = msg
                .target_region
                .clone()
                .unwrap_or_else(|| "conversation".to_string());
            if msg.parts.is_empty() {
                let tokens = leviath_core::estimate_tokens(&msg.content);
                let _ = window.add_typed_entry(
                    &region,
                    leviath_core::EntryKind::UserMessage,
                    msg.content.clone(),
                    tokens,
                );
                continue;
            }
            deliver_with_parts(&mut window, entity, &state.agent_id, &region, msg, &media);
        }
    }
}

/// Deliver a message that carries files: the text and every part bound for
/// the message's own region land as one entry, and a part naming another
/// region lands there on its own. A part the run cannot take (no store, over
/// the size ceiling, a region that refuses its type) is logged and dropped;
/// the text is still delivered, so the sender's words are never lost to a
/// bad attachment.
fn deliver_with_parts(
    window: &mut ContextWindow,
    entity: Entity,
    run_id: &str,
    region: &str,
    msg: AgentMessage,
    media: &crate::blob_store::MediaParams,
) {
    let (sources, _) = media.hydration_inputs(entity);
    let Some((store, registry)) = sources else {
        tracing::warn!(
            run_id,
            "[media] this world has no blob store; delivering the message text without its {} part(s)",
            msg.parts.len()
        );
        let tokens = leviath_core::estimate_tokens(&msg.content);
        let _ = window.add_typed_entry(
            region,
            leviath_core::EntryKind::UserMessage,
            msg.content.clone(),
            tokens,
        );
        return;
    };
    let sink = crate::context_setup::PartSink {
        store: store.as_ref(),
        registry: &registry,
        run_id,
        max_part_bytes: media.max_part_bytes(),
    };
    let (mine, elsewhere): (Vec<_>, Vec<_>) = msg
        .parts
        .into_iter()
        .partition(|p| p.region.as_deref().is_none_or(|r| r == region));
    let mut parts = Vec::new();
    if !msg.content.trim().is_empty() {
        parts.push(leviath_core::media::Part::text(msg.content.clone()));
    }
    for inbound in &mine {
        match sink.store_part(inbound) {
            Ok(part) => parts.push(part),
            Err(e) => tracing::warn!(run_id, "[media] dropped an attached part: {e}"),
        }
    }
    if parts.is_empty() {
        parts.push(leviath_core::media::Part::text(msg.content));
    }
    let content = leviath_core::region::EntryContent::from_parts(parts);
    let tokens = sink.tokens_for(&content);
    if let Err(e) = window.add_content_entry(
        region,
        leviath_core::EntryKind::UserMessage,
        content,
        tokens,
    ) {
        tracing::warn!(run_id, region, "[media] message refused: {e}");
    }
    for inbound in elsewhere {
        let target = inbound.region.clone().unwrap_or_default();
        let entry = sink.entry_for(&inbound);
        let landed = entry.and_then(|content| {
            let tokens = sink.tokens_for(&content);
            window
                .add_content_entry(
                    &target,
                    leviath_core::EntryKind::UserMessage,
                    content,
                    tokens,
                )
                .map_err(|e| e.to_string())
        });
        if let Err(e) = landed {
            tracing::warn!(run_id, region = %target, "[media] dropped an attached part: {e}");
        }
    }
}
