//! The mutations `--allow-admin` opens, and the gate in front of them.
//!
//! These change the machine rather than a run: adding an MCP server writes a
//! command into the config that Leviath then spawns, for this run and every
//! future one. The REST side answers 404 for them without `--allow-admin`,
//! because an unmounted route cannot be reached at all.
//!
//! A GraphQL schema has no "unmounted": a field is in the type or it is not,
//! and the type is built once. So the gate is two things together. The field is
//! invisible to introspection without the flag, so a client cannot discover it,
//! and the guard refuses it during execution, so a client that knows the name
//! anyway gets `FORBIDDEN` rather than the act.

use async_graphql::{Context, Guard, Object, SimpleObject};

use super::super::core::error::ServeError;
use super::error::{IntoGraphql, graphql_error};

/// Whether this server was started with `--allow-admin`.
///
/// Decided once, at startup, and put into the schema then. The flag is not on
/// `AppState` on purpose: a handler that consults a field is one refactor away
/// from forgetting to, where a decision made at build time is made once.
#[derive(Clone, Copy)]
pub(crate) struct AdminAccess(pub(crate) bool);

/// Whether the admin fields are visible to introspection.
///
/// Invisible is not a security boundary, the guard is. It is so a client
/// exploring the schema is not shown acts this server will refuse.
pub(crate) fn admin_visible(ctx: &Context<'_>) -> bool {
    ctx.data_opt::<AdminAccess>().is_some_and(|access| access.0)
}

/// Refuses an admin mutation on a server that was not started for them.
pub(crate) struct AdminGuard;

impl Guard for AdminGuard {
    async fn check(&self, ctx: &Context<'_>) -> async_graphql::Result<()> {
        match admin_visible(ctx) {
            true => Ok(()),
            false => Err(graphql_error(&ServeError::Forbidden(
                "this server was not started with --allow-admin, which is what opens the \
                 mutations that change the machine rather than a run"
                    .to_string(),
            ))),
        }
    }
}

/// What writing a mime row did.
#[derive(Debug, SimpleObject)]
pub(crate) struct MimeRowWritten {
    /// The row's key.
    pub(crate) mime_type: String,
    /// True when the row is new, false when an existing one was updated.
    pub(crate) created: bool,
}

/// The acts that change the machine.
///
/// Merged into the mutation root, so these read as ordinary mutations to a
/// client that is allowed to use them and do not exist to one that is not.
#[derive(Default)]
pub(crate) struct AdminMutation;

#[Object]
impl AdminMutation {
    /// Add an MCP server to the config.
    ///
    /// Remote code execution by construction: the command written here is what
    /// Leviath spawns, for this run and every future one. That is why the whole
    /// group is behind a flag rather than behind the API token alone.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn add_mcp_server(
        &self,
        #[graphql(desc = "Unique server name.")] name: String,
        #[graphql(desc = "The command, for a stdio server.")] command: Option<String>,
        #[graphql(desc = "The URL, for an HTTP server.")] url: Option<String>,
        #[graphql(desc = "Arguments for a stdio server.")] args: Option<Vec<String>>,
    ) -> async_graphql::Result<bool> {
        super::super::mcp::install_server(
            &name,
            command.as_deref(),
            url.as_deref(),
            args.unwrap_or_default(),
        )
        .gql()?;
        Ok(true)
    }

    /// Remove an MCP server from the config.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn remove_mcp_server(
        &self,
        #[graphql(desc = "The server to remove.")] name: String,
    ) -> async_graphql::Result<bool> {
        super::super::mcp::uninstall_server(&name).gql()?;
        Ok(true)
    }

    /// Add or update one row of the mime registry.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn put_mime_row(
        &self,
        #[graphql(desc = "The type or pattern this row covers.")] mime_type: String,
        #[graphql(desc = "The family providers key their encoders on.")] family: Option<String>,
        #[graphql(desc = "Whether the bytes are text, and may travel inline.")] text: Option<bool>,
        #[graphql(desc = "Extensions that imply this type, without the dot.")] extensions: Option<
            Vec<String>,
        >,
    ) -> async_graphql::Result<MimeRowWritten> {
        let written = super::super::mime::write_row(&mime_type, family, text, extensions).gql()?;
        Ok(MimeRowWritten {
            mime_type: written.mime_type,
            created: written.created,
        })
    }

    /// Remove a row from the mime registry.
    ///
    /// False when there was no such row, which is a fact about the registry
    /// rather than a failed request.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn delete_mime_row(
        &self,
        #[graphql(desc = "The row to remove.")] mime_type: String,
    ) -> async_graphql::Result<bool> {
        super::super::mime::remove_row_named(&mime_type).gql()
    }
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
