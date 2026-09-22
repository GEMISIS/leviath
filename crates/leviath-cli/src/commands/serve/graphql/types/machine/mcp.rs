//! One MCP server as this machine has it configured: how it is reached, and
//! where it stands on credentials.

use async_graphql::{ID, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::manifest::dependency::McpTransport;

/// One MCP server in the config.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct McpServer {
    /// `mcpServer:<name>`. A machine holds one server per name, so the name is
    /// the whole key.
    #[graphql(owned)]
    #[filter(orderable)]
    pub(crate) id: ID,
    /// Unique server name.
    #[filter(orderable)]
    pub(crate) name: String,
    /// How it is reached. Null when the configuration does not resolve to
    /// either transport, and `configError` says why.
    pub(crate) transport: Option<McpTransport>,
    /// The command for a stdio server, the URL for an HTTP one. Empty when the
    /// configuration does not resolve.
    #[filter(orderable)]
    pub(crate) endpoint: String,
    /// The program a stdio server is, as the config spells it. Null for an
    /// HTTP server, and for an entry that names neither.
    pub(crate) command: Option<String>,
    /// Where an HTTP server is. Null for a stdio server.
    pub(crate) url: Option<String>,
    /// The arguments a stdio server is spawned with, in order. Empty for an
    /// HTTP one.
    pub(crate) args: Vec<String>,
    /// The names of the headers an HTTP server is sent, without their values:
    /// an `Authorization` header here is a credential.
    pub(crate) header_names: Vec<String>,
    /// The names of the environment variables a stdio server is spawned with,
    /// without their values, for the same reason.
    pub(crate) env_names: Vec<String>,
    /// Why the configuration does not resolve, when it does not. Null for a
    /// server that does.
    pub(crate) config_error: Option<String>,
    /// Where it stands on credentials.
    pub(crate) auth: McpAuth,
}

impl super::super::super::connection::Paged for McpServer {
    const NAME: &'static str = "McpServer";
}

impl McpServer {
    /// Describe one server this machine has configured.
    pub(crate) fn from_info(info: super::super::super::super::mcp::McpServerInfo) -> Self {
        Self {
            id: super::super::super::node::mcp_server_id(&info.name),
            name: info.name,
            transport: transport_of(&info.transport),
            endpoint: info.endpoint,
            command: info.command,
            url: info.url,
            args: info.args,
            header_names: info.header_names,
            env_names: info.env_names,
            config_error: info.config_error,
            auth: McpAuth::from_wire(&info.auth),
        }
    }
}

/// The transport word the server description carries, as a value.
///
/// One enum for both sides of the schema: a blueprint declares the same two
/// transports a configured server is reached over. "Neither" is not a third
/// transport, it is the absence of one, so it is `None` here and the reason is
/// on `configError`.
pub(crate) fn transport_of(word: &str) -> Option<McpTransport> {
    match word {
        "stdio" => Some(McpTransport::Stdio),
        "http" => Some(McpTransport::Http),
        _ => None,
    }
}

/// Where a server stands on credentials.
#[mirror]
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
