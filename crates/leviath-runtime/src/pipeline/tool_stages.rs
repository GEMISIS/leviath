//! Keeping the tool service's view of each agent in step with the stage it is
//! actually in, including refreshing dynamically advertised tools.

use super::*;

/// Notify the [`ToolService`] of every agent that just entered a stage (tagged
/// with [`StageJustEntered`] by the transition systems), so it can re-sync that
/// agent's per-stage tool permissions, then clear the tag. Runs after the
/// transition systems each tick.
pub(crate) fn sync_tool_stages(
    service: Res<ToolServiceRes>,
    entered: Query<(Entity, &StageJustEntered)>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, stage) in entered.iter() {
        crate::tick_scope::enter(entity);
        service.0.sync_stage(entity, stage.index, &stage.name);
        commands.entity(entity).remove::<StageJustEntered>();
    }
}

/// Re-advertise an agent's tools mid-run: when tagged [`ToolsNeedRefresh`], ask
/// the tool service for this stage's freshly-resolved tool defs and, if it
/// returns a set, write it into the live [`StageInference`] (what the next
/// inference request advertises, read fresh by `build_request`) and the matching
/// [`StageInferences`] catalog entry (so a later revisit of this stage keeps the
/// updated set). Always consumes the marker. This is the mechanism behind
/// mid-run dynamic tool discovery and lazily-listed MCP tools.
pub(crate) fn refresh_advertised_tools(
    service: Res<ToolServiceRes>,
    mut agents: Query<
        (
            Entity,
            &StageCursor,
            &mut StageInference,
            &mut StageInferences,
        ),
        With<ToolsNeedRefresh>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, cursor, mut si, mut sis) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        advertise_refreshed(&service, entity, cursor.index, &mut si, &mut sis);
        commands.entity(entity).remove::<ToolsNeedRefresh>();
    }
}

/// What a refreshing system needs off an agent: where it is, what it advertises
/// now, and the catalog the set has to be written back into.
///
/// Named because two systems take exactly this, and spelling it twice is what
/// the clippy complaint about it is really about.
type Advertised<'a> = (
    Entity,
    &'a StageCursor,
    Mut<'a, StageInference>,
    Mut<'a, StageInferences>,
);

/// Ask the service for this stage's tools and write them where both the next
/// request and a later revisit will read them.
///
/// One function for the two systems that refresh, because writing only the live
/// `StageInference` is a bug that hides until the stage is re-entered: the
/// catalog would still hold the set the run started with.
fn advertise_refreshed(
    service: &Res<ToolServiceRes>,
    entity: Entity,
    stage_index: usize,
    si: &mut StageInference,
    sis: &mut StageInferences,
) {
    let Some(tools) = service.0.refresh_tools(entity, stage_index) else {
        return;
    };
    si.tools = tools.clone();
    if let Some(slot) = sis.0.get_mut(stage_index) {
        slot.tools = tools;
    }
}

/// Look for tools again before a batch is dispatched, for an agent whose
/// blueprint asks for it.
///
/// Runs between the response that named the calls and the dispatch that sends
/// them, because the advertised set is what dispatch refuses an unoffered call
/// against: a tool that arrived since this turn was built is callable in it only
/// if the set is rewritten here.
///
/// Note what it cannot do. Every call in one batch is checked before any of them
/// runs, so a batch that writes a tool and calls it still has the call refused -
/// the write has not happened yet. What this catches is a tool that arrived
/// without the service being told: from a shell command, a script tool, or
/// another agent sharing the workdir.
///
/// Gated on [`ToolService::scan_stale`], so the ordinary batch costs a `stat`
/// per scanned directory and nothing else. No marker is consumed: the agent
/// carries [`RescanBeforeDispatch`] for its whole run, and this runs once per
/// batch it dispatches.
pub(crate) fn rescan_before_dispatch(
    service: Res<ToolServiceRes>,
    mut agents: Query<Advertised, (With<ReadyForTools>, With<RescanBeforeDispatch>)>,
) {
    crate::tick_scope::clear();
    for (entity, cursor, mut si, mut sis) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if !service.0.scan_stale(entity) {
            continue;
        }
        advertise_refreshed(&service, entity, cursor.index, &mut si, &mut sis);
    }
}

/// Poll each `dynamic_tools` agent for a pending tool re-scan and, when the tool
/// service reports one, tag it [`ToolsNeedRefresh`] so [`refresh_advertised_tools`]
/// re-advertises before its next turn. Only agents carrying [`DynamicTools`] are
/// queried, so static agents (the default) cost nothing.
pub(crate) fn poll_dynamic_tool_refresh(
    service: Res<ToolServiceRes>,
    agents: Query<Entity, With<DynamicTools>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for entity in agents.iter() {
        crate::tick_scope::enter(entity);
        if service.0.wants_refresh(entity) {
            commands.entity(entity).insert(ToolsNeedRefresh);
        }
    }
}
