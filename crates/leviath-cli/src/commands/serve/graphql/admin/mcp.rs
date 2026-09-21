//! The `addMcpServer`, `removeMcpServer`, `testMcpServer` and
//! `loginMcpServer` fields: writing an MCP server into the config, and
//! checking that what is written actually starts.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::config_input::EnvEntryInput;
use super::super::error::IntoGraphql;

/// Add an MCP server to the config.
///
/// Remote code execution by construction: the command written here is what
/// Leviath spawns, for this run and every future one. That is why the whole
/// group is behind a flag rather than behind the API token alone.
pub(crate) async fn add_mcp_server(
    name: String,
    command: Option<String>,
    url: Option<String>,
    args: Option<Vec<String>>,
    headers: Option<Vec<EnvEntryInput>>,
) -> async_graphql::Result<bool> {
    super::super::super::mcp::install_server(
        name,
        command,
        url,
        args.unwrap_or_default(),
        headers
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.name, entry.value))
            .collect(),
    )
    .gql()?;
    Ok(true)
}

/// Remove an MCP server from the config.
pub(crate) async fn remove_mcp_server(name: String) -> async_graphql::Result<bool> {
    super::super::super::mcp::uninstall_server(&name).gql()?;
    Ok(true)
}

/// Connect to an MCP server and list what it advertises.
///
/// The only honest answer to "does this server work": a config that parses
/// proves nothing about a program that will not start.
pub(crate) async fn test_mcp_server(
    ctx: &Context<'_>,
    name: String,
) -> async_graphql::Result<Vec<String>> {
    let state = ctx.data_unchecked::<AppState>();
    super::super::super::mcp::tools_of(state, &name).await.gql()
}

/// What signing in to an MCP server ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum McpLoginStatus {
    /// A grant was obtained and stored.
    Authenticated,
    /// The server wants no OAuth, so there was nothing to store. A success: the
    /// question was whether a sign-in was needed.
    NotRequired,
}
impl From<super::super::super::mcp::LoginStatus> for McpLoginStatus {
    /// Its own impl rather than a match inside the resolver: reaching that
    /// resolver means completing an OAuth handshake against a real server, and
    /// the mapping is worth checking without one.
    fn from(status: super::super::super::mcp::LoginStatus) -> Self {
        match status {
            super::super::super::mcp::LoginStatus::Authenticated => Self::Authenticated,
            super::super::super::mcp::LoginStatus::NotRequired => Self::NotRequired,
        }
    }
}

/// Sign in to an MCP server that wants OAuth.
///
/// `NOT_REQUIRED` is a success, not a failure: the question was whether a
/// sign-in was needed, and the answer is no. Opens a browser on the serving
/// host, like the provider sign-in.
pub(crate) async fn login_mcp_server(
    ctx: &Context<'_>,
    name: String,
) -> async_graphql::Result<McpLoginStatus> {
    let state = ctx.data_unchecked::<AppState>();
    let status = super::super::super::mcp::signed_in(state, &name)
        .await
        .gql()?;
    Ok(McpLoginStatus::from(status))
}
