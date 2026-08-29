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
        if let Some(tools) = service.0.refresh_tools(entity, cursor.index) {
            si.tools = tools.clone();
            // Keep the catalog entry in sync so re-entering this stage advertises
            // the same refreshed set.
            if let Some(slot) = sis.0.get_mut(cursor.index) {
                slot.tools = tools;
            }
        }
        commands.entity(entity).remove::<ToolsNeedRefresh>();
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
