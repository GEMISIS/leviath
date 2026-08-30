//! `GET /api/tools`: what tools an agent on this machine can actually use.
//!
//! Nothing else in the API answered that, so a client building a tool picker
//! had to ship a list, and a shipped list is wrong in both directions: a global
//! `.rhai` the user wrote never appears, and a tool the list names may not exist
//! on the machine the client is talking to.
//!
//! The discovery itself lives in [`crate::tool_inventory`], shared with the
//! blueprint lint, because both are asking the same question and a second copy
//! of the rules would have drifted.

use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::types::{ApiError, AppState, err};
use crate::tool_inventory::ToolInventory;

/// The directory of the agent called `name`, refusing a name that is not a
/// single safe path component.
///
/// The name is looked up in the same catalog `GET /api/blueprints` lists, so an
/// agent found through `config.agent_paths` resolves to where it actually is.
/// A plain `agents_dir().join(name)` would point every caller at
/// `~/.leviath/agents/<name>/` instead: an agent under development in a
/// configured path is listed and can be spawned, but its `tools/` and hooks
/// live beside it, and a `PUT /api/scripts/...?agent=` would write into an
/// empty directory nothing loads. A name the catalog does not know still falls
/// back to the installed directory, so a new agent can be created there.
///
/// The same gate, on the same directory, that `blueprint_dir` applies in
/// `blueprints.rs`: `Path::join` neither normalizes `..` nor resists an
/// absolute path, and this name arrives in a query string. Shared from here
/// because the tools and scripts routes both take an `?agent=` and both would
/// otherwise write their own copy of the check.
pub(super) fn agent_dir(config: &crate::config::Config, name: &str) -> Result<PathBuf, ApiError> {
    if !leviath_core::is_safe_path_component(name) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid agent name '{name}': names may contain only letters, digits, \
                 '.', '_' and '-'"
            ),
        ));
    }
    let known = super::blueprints::discover_blueprints(config)
        .into_iter()
        .find(|b| b.name == name)
        .map(|b| PathBuf::from(b.path));
    Ok(known.unwrap_or_else(|| super::blueprints::agents_dir().join(name)))
}

/// Query parameters for `GET /api/tools`.
#[derive(Debug, Deserialize)]
pub(super) struct ToolsQuery {
    /// Include this agent's own `tools/` alongside the built-ins and the global
    /// directory. Absent means the machine-wide answer.
    agent: Option<String>,
}

/// One tool in the listing.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ToolItem {
    /// The name a blueprint lists in `available_tools` and the model calls.
    pub(super) name: String,
    /// `builtin`, `subagent`, `agent` or `global`.
    pub(super) source: String,
    /// The `.rhai` file behind it, for script-backed tools only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) path: Option<String>,
    /// The agent that owns it, for agent-scoped tools only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent: Option<String>,
}

/// One `.rhai` file that was found and could not be offered.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SkippedItem {
    /// The file that was passed over.
    pub(super) path: String,
    /// Why: a compile error, bad annotations, or a name already taken.
    pub(super) reason: String,
    /// Which directory it was found in, `agent` or `global`.
    pub(super) source: String,
}

/// The body of `GET /api/tools`.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ToolsResp {
    /// Everything that will work if a blueprint names it.
    pub(super) tools: Vec<ToolItem>,
    /// Everything that looked like a tool and will not work, with the reason.
    /// A tool missing because its file has a syntax error is exactly what a
    /// picker should be able to show, and the reason is the only thing that
    /// tells it apart from a tool nobody ever wrote.
    pub(super) skipped: Vec<SkippedItem>,
}

/// `GET /api/tools[?agent=<name>]`: the tool inventory of this machine.
///
/// MCP tools are deliberately absent. They depend on a server being reachable
/// rather than on anything installed here, and `/api/mcp/servers/{name}`
/// already answers for them.
pub(super) async fn list_tools(
    State(state): State<AppState>,
    Query(q): Query<ToolsQuery>,
) -> Result<Json<ToolsResp>, ApiError> {
    let dir = match q.agent.as_deref() {
        Some(name) => Some(agent_dir(&state.current_config(), name)?),
        None => None,
    };
    let inventory = ToolInventory::discover(dir.as_deref(), q.agent.as_deref());

    let tools = inventory
        .tools
        .into_iter()
        .map(|t| ToolItem {
            name: t.name,
            source: t.source.as_str().to_string(),
            path: t.path.map(|p| p.display().to_string()),
            agent: t.agent,
        })
        .collect();
    let skipped = inventory
        .skipped
        .into_iter()
        .map(|s| SkippedItem {
            path: s.path.display().to_string(),
            reason: s.reason,
            source: s.source.as_str().to_string(),
        })
        .collect();

    Ok(Json(ToolsResp { tools, skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::path::Path;
    use tower::ServiceExt;

    use super::super::testutil::{state_with_agent_paths, with_home};
    use crate::config::Config;

    fn write_tool(dir: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("the directory");
        std::fs::write(dir.join(file), body).expect("the script");
    }

    async fn get_tools(uri: &str) -> (StatusCode, serde_json::Value) {
        get_tools_with_paths(Vec::new(), uri).await
    }

    /// `get_tools`, against a server whose config lists `agent_paths`.
    async fn get_tools_with_paths(
        agent_paths: Vec<std::path::PathBuf>,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let app = Router::new()
            .route("/api/tools", get(list_tools))
            .with_state(state_with_agent_paths(agent_paths));
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// With no `?agent=`, the answer is about the machine: built-ins, sub-agent
    /// tools, and whatever is in the global directory.
    #[tokio::test]
    async fn listing_without_an_agent_covers_the_machine() {
        with_home(|home| async move {
            write_tool(
                &home.join(".leviath").join("tools"),
                "summarize.rhai",
                "// @tool summarize\n// @description sums\n1",
            );
            let (status, body) = get_tools("/api/tools").await;

            assert_eq!(status, StatusCode::OK);
            let tools = body["tools"].as_array().expect("a tools array");
            let summarize = tools
                .iter()
                .find(|t| t["name"] == "summarize")
                .expect("the global tool");
            assert_eq!(summarize["source"], "global");
            let path = summarize["path"].as_str().expect("a path");
            assert!(path.ends_with("summarize.rhai"), "{path}");
            assert!(tools.iter().any(|t| t["source"] == "builtin"));
            assert!(tools.iter().any(|t| t["source"] == "subagent"));
            assert!(tools.iter().all(|t| t["source"] != "agent"));
            // A built-in has no file behind it, so it carries no `path`.
            let builtin = tools
                .iter()
                .find(|t| t["source"] == "builtin")
                .expect("a built-in");
            assert!(builtin.get("path").is_none());
            assert!(builtin.get("agent").is_none());
        })
        .await;
    }

    /// `?agent=` adds that agent's own `tools/`, labelled with the agent it
    /// belongs to - which is the difference between "everything here has this"
    /// and "only this agent does".
    #[tokio::test]
    async fn listing_with_an_agent_adds_that_agents_own_tools() {
        with_home(|home| async move {
            let agent = home.join(".leviath").join("agents").join("researcher");
            write_tool(
                &agent.join("tools"),
                "web_search.rhai",
                "// @tool web_search\n// @description searches\n1",
            );
            let (status, body) = get_tools("/api/tools?agent=researcher").await;

            assert_eq!(status, StatusCode::OK);
            let tools = body["tools"].as_array().expect("a tools array");
            let own = tools
                .iter()
                .find(|t| t["name"] == "web_search")
                .expect("the agent's own tool");
            assert_eq!(own["source"], "agent");
            assert_eq!(own["agent"], "researcher");
            let path = own["path"].as_str().expect("a path");
            assert!(path.ends_with("web_search.rhai"), "{path}");
        })
        .await;
    }

    /// A script that will not compile is reported rather than dropped, because
    /// "your file has a syntax error" and "you never wrote that file" look
    /// identical from outside the daemon.
    #[tokio::test]
    async fn a_script_that_fails_to_compile_is_reported_as_skipped() {
        with_home(|home| async move {
            let agent = home.join(".leviath").join("agents").join("broken");
            write_tool(&agent.join("tools"), "bad.rhai", "// no directive\nlet");
            let (status, body) = get_tools("/api/tools?agent=broken").await;

            assert_eq!(status, StatusCode::OK);
            let skipped = body["skipped"].as_array().expect("a skipped array");
            assert_eq!(skipped.len(), 1);
            let path = skipped[0]["path"].as_str().expect("a path");
            assert!(path.ends_with("bad.rhai"), "{path}");
            assert_eq!(skipped[0]["source"], "agent");
            assert!(!skipped[0]["reason"].as_str().expect("a reason").is_empty());
            let tools = body["tools"].as_array().expect("a tools array");
            assert!(tools.iter().all(|t| t["name"] != "bad"));
        })
        .await;
    }

    /// The agent name is joined onto a directory, so it goes through the same
    /// component check the blueprint routes use. `..` is a traversal, not a name.
    #[tokio::test]
    async fn a_traversing_agent_name_is_rejected() {
        with_home(|_home| async move {
            let (status, body) = get_tools("/api/tools?agent=..%2F..%2Fetc").await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let message = body["error"].as_str().expect("an error message");
            assert!(message.contains("Invalid agent name"), "{message}");
        })
        .await;
    }

    #[tokio::test]
    async fn agent_dir_accepts_a_plain_name() {
        with_home(|home| async move {
            let dir = agent_dir(&Config::default(), "researcher").expect("a plain name resolves");
            assert!(dir.starts_with(home.join(".leviath").join("agents")));
        })
        .await;
    }

    /// An agent listed because its directory is under `config.agent_paths`
    /// resolves to that directory, not to an empty `~/.leviath/agents/<name>`,
    /// so the `tools/` beside it are the ones listed.
    #[tokio::test]
    async fn an_agent_from_a_configured_path_lists_its_own_tools() {
        with_home(|home| async move {
            let workspace = home.join("workspace");
            let agent = workspace.join("researcher");
            std::fs::create_dir_all(&agent).expect("the agent directory");
            std::fs::write(
                agent.join("agent.leviath"),
                "[agent]\nname = \"researcher\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 \n[stages.main]\nsystem_prompt = \"go\"\n",
            )
            .expect("the manifest");
            write_tool(
                &agent.join("tools"),
                "web_search.rhai",
                "// @tool web_search\n// @description searches\n1",
            );
            let config = Config {
                agent_paths: vec![workspace.clone()],
                ..Default::default()
            };
            assert_eq!(
                agent_dir(&config, "researcher").expect("a known name resolves"),
                agent
            );

            let (status, body) =
                get_tools_with_paths(vec![workspace], "/api/tools?agent=researcher").await;
            assert_eq!(status, StatusCode::OK);
            let tools = body["tools"].as_array().expect("a tools array");
            let own = tools
                .iter()
                .find(|t| t["name"] == "web_search")
                .expect("the agent's own tool");
            assert_eq!(own["source"], "agent");
            assert_eq!(own["agent"], "researcher");
            let path = own["path"].as_str().expect("a path");
            assert!(path.starts_with(&agent.display().to_string()), "{path}");
        })
        .await;
    }
}
