//! One MCP server as this machine has it configured: how it is reached, and
//! where it stands on credentials.

use async_graphql::{ID, SimpleObject};

/// One MCP server in the config.
#[derive(Debug, SimpleObject)]
pub(crate) struct McpServer {
    /// `mcpServer:<name>`. A machine holds one server per name, so the name is
    /// the whole key.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// Unique server name.
    pub(crate) name: String,
    /// How it is reached.
    pub(crate) transport: McpServerTransport,
    /// The command for a stdio server, the URL for an HTTP one. Empty when the
    /// configuration does not resolve.
    pub(crate) endpoint: String,
    /// Why the configuration does not resolve, when it does not. Null for a
    /// server that does.
    pub(crate) config_error: Option<String>,
    /// Where it stands on credentials.
    pub(crate) auth: McpAuth,
}

impl McpServer {
    /// Describe one server this machine has configured.
    pub(crate) fn from_info(info: super::super::super::super::mcp::McpServerInfo) -> Self {
        Self {
            id: super::super::super::node::mcp_server_id(&info.name),
            name: info.name,
            transport: McpServerTransport::from_wire(&info.transport),
            endpoint: info.endpoint,
            config_error: info.config_error,
            auth: McpAuth::from_wire(&info.auth),
        }
    }
}

/// How a configured MCP server on this machine is reached.
///
/// Its own type rather than the manifest's `McpTransport`: a blueprint
/// declares what it wants, and this answers what a configuration on this
/// machine resolved to - including `INVALID`, which no blueprint can declare.
/// `configError` says why it did not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum McpServerTransport {
    /// A command this daemon spawns and talks to over its pipes.
    Stdio,
    /// A URL this daemon calls.
    Http,
    /// Neither: the configuration did not resolve.
    Invalid,
}

impl McpServerTransport {
    /// Read the word the server description carries.
    ///
    /// Anything else is `INVALID`, which is what a word this build does not
    /// know amounts to: a transport it cannot use.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "stdio" => Self::Stdio,
            "http" => Self::Http,
            _ => Self::Invalid,
        }
    }
}

/// Where a server stands on credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum McpAuth {
    /// A stdio server, which has nobody to log in to.
    NotApplicable,
    /// An HTTP server with no credential of any kind.
    None,
    /// An `Authorization` header from the config. A credential, so no login is
    /// offered for it.
    Header,
    /// Signed in, and the token has not expired.
    Authenticated,
    /// Signed in once; the token has expired and the server needs another.
    Expired,
}

impl McpAuth {
    /// Read the word the server description carries.
    ///
    /// An unknown word reads as `NONE`: the safe reading, since it is the one
    /// that offers a login rather than assuming a credential is in place.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "n/a" => Self::NotApplicable,
            "header" => Self::Header,
            "authenticated" => Self::Authenticated,
            "expired" => Self::Expired,
            _ => Self::None,
        }
    }
}
