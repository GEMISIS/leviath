//! The hint for a validator tool that would have to ask: what would let
//! that call run, for the layers that put it at `ask`.

use crate::daemon::tool_service::AgentToolState;

/// What to change so the validator tool `tool`, called from stage
/// `stage_index`, runs without asking. `blueprint` is the installed
/// blueprint the run came from, if any.
///
/// A graph's `tool_permissions` take effect only for a tool a graph may
/// loosen (see `resolve_policy`), so asking the caller to write one is right
/// only then. For any other tool the spawn's own `allow` lets it run, and so
/// does the user's `[agent_tool_permissions.<name>]` for an installed
/// blueprint. `[security] allow_blueprint_permissions` lets the graph's own
/// `allow` count, so it is named only where the graph says one and the user
/// has no setting of their own for the tool.
pub(super) fn ask_hint(
    tools: &AgentToolState,
    stage_index: usize,
    tool: &str,
    blueprint: Option<&str>,
) -> String {
    let global = tools.global_perms.get();
    let graph_may_allow =
        crate::tools::graph_may_allow(&global, tool, tools.blueprint_may_loosen());
    let leave = "or name a tool the stage may call unprompted";
    if graph_may_allow {
        return format!("set `{tool} = \"allow\"` in tool_permissions, {leave}");
    }
    let said_allow = tools
        .stage_perms_by_index
        .get(stage_index)
        .is_some_and(|stage| crate::tools::graph_says_allow(stage, &tools.agent_perms, tool));
    let lead = match said_allow {
        true => format!(
            "`{tool} = \"allow\"` in tool_permissions has no effect, because a graph cannot \
             allow this tool by itself"
        ),
        false => "a graph cannot allow this tool by itself".to_string(),
    };
    let mut ways =
        vec!["name it in this spawn's `launch.allow` (`--allow` on the command line)".to_string()];
    if let Some(name) = blueprint {
        ways.push(format!(
            "set `{tool} = \"allow\"` under `[agent_tool_permissions.{name}]` in the user's config"
        ));
        if said_allow && !crate::tools::user_sets(&global, tool) {
            ways.push(
                "turn on `[security] allow_blueprint_permissions` in the user's config".to_string(),
            );
        }
    }
    format!("{lead}; {}, {leave}", ways.join(", or "))
}
