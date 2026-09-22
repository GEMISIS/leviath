//! The MCP server fields: writing one into the config, changing it, taking it
//! out, and checking that what is written actually starts.

use std::collections::HashMap;

use async_graphql::{Context, ID, InputObject, OneofObject, SimpleObject};

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::KeyValueWrite;
use super::super::types::machine::McpServer;

/// How a stdio MCP server is reached: a program this daemon spawns.
#[derive(Debug, InputObject)]
pub(crate) struct McpStdioWrite {
    /// The program to run.
    pub(crate) command: String,
    /// Its arguments, passed without shell interpretation.
    pub(crate) args: Option<Vec<String>>,
}

/// How an HTTP MCP server is reached: a URL this daemon calls.
#[derive(Debug, InputObject)]
pub(crate) struct McpHttpWrite {
    /// Where the server is.
    pub(crate) url: String,
    /// Headers sent with every request. An `Authorization` header here is a
    /// credential, so the server needs no separate sign-in. Read back as
    /// `headerNames`, never as values.
    pub(crate) headers: Option<Vec<KeyValueWrite>>,
}

/// How an MCP server is reached. Exactly one.
#[derive(Debug, OneofObject)]
pub(crate) enum McpTransportWrite {
    /// A program this daemon spawns and talks to over its pipes.
    Stdio(McpStdioWrite),
    /// A URL this daemon calls.
    Http(McpHttpWrite),
}

/// One MCP server, as a write sends it.
#[derive(Debug, InputObject)]
pub(crate) struct McpServerWrite {
    /// Unique server name.
    pub(crate) name: String,
    /// How it is reached.
    pub(crate) transport: McpTransportWrite,
    /// Variables added to a stdio server's environment. Read back as
    /// `envNames`, never as values.
    pub(crate) env: Option<Vec<KeyValueWrite>>,
}

/// One key-and-value list as the map the config holds.
fn pairs(entries: Option<Vec<KeyValueWrite>>) -> HashMap<String, String> {
    entries
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect()
}

/// A server write as the four things the config entry is built from.
struct Parts {
    /// The program, for a stdio server.
    command: Option<String>,
    /// The URL, for an HTTP one.
    url: Option<String>,
    /// The arguments, for a stdio server.
    args: Vec<String>,
    /// The headers, for an HTTP one.
    headers: HashMap<String, String>,
}

impl McpServerWrite {
    /// The name, and the entry's own fields.
    fn split(self) -> (String, HashMap<String, String>, Parts) {
        let parts = match self.transport {
            McpTransportWrite::Stdio(stdio) => Parts {
                command: Some(stdio.command),
                url: None,
                args: stdio.args.unwrap_or_default(),
                headers: HashMap::new(),
            },
            McpTransportWrite::Http(http) => Parts {
                command: None,
                url: Some(http.url),
                args: Vec::new(),
                headers: pairs(http.headers),
            },
        };
        (self.name, pairs(self.env), parts)
    }
}

/// Which server a write is about.
#[derive(Debug, InputObject)]
pub(crate) struct CreateMcpServerRequest {
    /// The server to write.
    pub(crate) server: McpServerWrite,
}

/// The server that was written.
#[derive(Debug, SimpleObject)]
pub(crate) struct CreateMcpServerResult {
    /// The server as the config now holds it.
    pub(crate) mcp_server: McpServer,
}

/// The server to replace, whole.
#[derive(Debug, InputObject)]
pub(crate) struct UpdateMcpServerRequest {
    /// The server, named by the `name` it is already under.
    pub(crate) server: McpServerWrite,
}

/// The server as it now stands.
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateMcpServerResult {
    /// The server as the config now holds it.
    pub(crate) mcp_server: McpServer,
}

/// Which server to take out.
#[derive(Debug, InputObject)]
pub(crate) struct DeleteMcpServerRequest {
    /// The server, by name.
    pub(crate) name: String,
}

/// What was taken out.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeleteMcpServerResult {
    /// The node id the server had.
    pub(crate) deleted_id: ID,
}

/// Which server to connect to.
#[derive(Debug, InputObject)]
pub(crate) struct CheckMcpServerRequest {
    /// The server, by name.
    pub(crate) name: String,
}

/// What a server advertises when it is asked.
#[derive(Debug, SimpleObject)]
pub(crate) struct CheckMcpServerResult {
    /// The server that was asked.
    pub(crate) mcp_server: McpServer,
    /// The tool names it advertises, as it spells them. Names rather than
    /// tools: this asks the server, and what comes back is a list of names.
    pub(crate) tool_names: Vec<String>,
}

/// Which server to sign in to.
#[derive(Debug, InputObject)]
pub(crate) struct SignInMcpServerRequest {
    /// The server, by name.
    pub(crate) name: String,
}

/// What signing in came to.
#[derive(Debug, SimpleObject)]
pub(crate) struct SignInMcpServerResult {
    /// The server that was signed in to.
    pub(crate) mcp_server: McpServer,
    /// Whether a grant was stored, or none was wanted.
    pub(crate) status: McpLoginStatus,
}

/// One server as this schema answers with it: the entry the writer or the
/// connection resolved, with its transport and auth state filled in.
///
/// From the entry rather than by name: every caller here is holding the entry
/// already, so there is no lookup to miss and no absent case to invent an
/// answer for.
fn described(state: &AppState, server: &leviath_mcp::MCPServerConfig) -> McpServer {
    McpServer::from_info(super::super::super::mcp::described(state, server))
}

/// Add an MCP server to the config.
///
/// Remote code execution by construction: the command written here is what
/// Leviath spawns, for this run and every future one. That is why the whole
/// group is behind a flag rather than behind the API token alone.
pub(crate) async fn create_mcp_server(
    ctx: &Context<'_>,
    request: CreateMcpServerRequest,
) -> async_graphql::Result<CreateMcpServerResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (name, env, parts) = request.server.split();
    let written = super::super::super::mcp::install_server(
        name,
        parts.command,
        parts.url,
        parts.args,
        env,
        parts.headers,
    )
    .gql()?;
    Ok(CreateMcpServerResult {
        mcp_server: described(state, &written),
    })
}

/// Replace an MCP server's configuration, whole.
///
/// Whole rather than field by field: the entry is what gets spawned, and an
/// edit that left half of a previous transport behind would be a server nobody
/// wrote. A name nothing is configured under is a miss.
pub(crate) async fn update_mcp_server(
    ctx: &Context<'_>,
    request: UpdateMcpServerRequest,
) -> async_graphql::Result<UpdateMcpServerResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (name, env, parts) = request.server.split();
    let written = super::super::super::mcp::update_server(
        name,
        parts.command,
        parts.url,
        parts.args,
        env,
        parts.headers,
    )
    .gql()?;
    Ok(UpdateMcpServerResult {
        mcp_server: described(state, &written),
    })
}

/// Remove an MCP server from the config, and its stored credential with it.
pub(crate) async fn delete_mcp_server(
    request: DeleteMcpServerRequest,
) -> async_graphql::Result<DeleteMcpServerResult> {
    super::super::super::mcp::uninstall_server(&request.name).gql()?;
    Ok(DeleteMcpServerResult {
        deleted_id: super::super::node::mcp_server_id(&request.name),
    })
}

/// Connect to an MCP server and list what it advertises.
///
/// The only honest answer to "does this server work": a config that parses
/// proves nothing about a program that will not start.
pub(crate) async fn check_mcp_server(
    ctx: &Context<'_>,
    request: CheckMcpServerRequest,
) -> async_graphql::Result<CheckMcpServerResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (tool_names, server) = super::super::super::mcp::tools_of(state, &request.name)
        .await
        .gql()?;
    Ok(CheckMcpServerResult {
        mcp_server: described(state, &server),
        tool_names,
    })
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
pub(crate) async fn sign_in_mcp_server(
    ctx: &Context<'_>,
    request: SignInMcpServerRequest,
) -> async_graphql::Result<SignInMcpServerResult> {
    let state = ctx.data_unchecked::<AppState>();
    let (status, server) = super::super::super::mcp::signed_in(state, &request.name)
        .await
        .gql()?;
    Ok(SignInMcpServerResult {
        mcp_server: described(state, &server),
        status: McpLoginStatus::from(status),
    })
}
