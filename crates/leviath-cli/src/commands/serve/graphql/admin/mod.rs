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
//!
//! One file per concern: [`config`] for the one write onto the daemon's own
//! config, [`mcp`] for an MCP server's config entry, [`mime`] for the mime
//! registry, [`scripts`] for a registered script, [`yolo`] for one profile in
//! the profiles file, [`providers`] for subscription sign-in and endpoint
//! checks, and
//! [`system`] for the acts that touch the machine itself: updates, live
//! diagnostics, and making a directory. `AdminMutation` stays one
//! `#[Object] impl` with one field per method, for the same reason `Query`
//! does: this schema's field order does not sort into those seven groups, so
//! `MergedObject` cannot reproduce it, and each method here is a one-line
//! delegation into its group's module instead.

use async_graphql::{Context, Guard, Object};

use super::config_input::UpdateConfigRequest;
use super::error::graphql_error;

use super::super::core::error::ServeError;

pub(crate) mod config;
pub(crate) mod mcp;
pub(crate) mod mime;
pub(crate) mod providers;
pub(crate) mod scripts;
pub(crate) mod system;
pub(crate) mod yolo;

use config::UpdateConfigResult;
use mcp::{
    CheckMcpServerRequest, CheckMcpServerResult, CreateMcpServerRequest, CreateMcpServerResult,
    DeleteMcpServerRequest, DeleteMcpServerResult, SignInMcpServerRequest, SignInMcpServerResult,
    UpdateMcpServerRequest, UpdateMcpServerResult,
};
use mime::{DeleteMimeRowRequest, DeleteMimeRowResult, UpsertMimeRowRequest, UpsertMimeRowResult};
use providers::{
    CheckEndpointRequest, CheckEndpointResult, CheckProviderRequest, CheckProviderResult,
    SignInProviderRequest, SignInProviderResult, SignOutProviderRequest, SignOutProviderResult,
};
use scripts::{DeleteScriptRequest, DeleteScriptResult, UpsertScriptRequest, UpsertScriptResult};
use system::{
    CheckMachineResult, CreateDirectoryRequest, CreateDirectoryResult, StartUpdateRequest,
    StartUpdateResult,
};
use yolo::{
    DeleteYoloProfileRequest, DeleteYoloProfileResult, UpsertYoloProfileRequest,
    UpsertYoloProfileResult,
};

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
    async fn create_mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server to write.")] request: CreateMcpServerRequest,
    ) -> async_graphql::Result<CreateMcpServerResult> {
        mcp::create_mcp_server(ctx, request).await
    }

    /// Replace an MCP server's configuration, whole.
    ///
    /// Whole rather than field by field: the entry is what gets spawned, and an
    /// edit that left half of a previous transport behind would describe a
    /// server nobody wrote.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn update_mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server, named by the name it is already under.")]
        request: UpdateMcpServerRequest,
    ) -> async_graphql::Result<UpdateMcpServerResult> {
        mcp::update_mcp_server(ctx, request).await
    }

    /// Remove an MCP server from the config, and its stored credential with it.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn delete_mcp_server(
        &self,
        #[graphql(desc = "The server to remove.")] request: DeleteMcpServerRequest,
    ) -> async_graphql::Result<DeleteMcpServerResult> {
        mcp::delete_mcp_server(request).await
    }

    /// Add or update one row of the mime registry.
    ///
    /// Every field but the key is optional, because a row says only what it
    /// changes: what a field leaves out stays as whatever broader row already
    /// covers the type.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn upsert_mime_row(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The row to write.")] request: UpsertMimeRowRequest,
    ) -> async_graphql::Result<UpsertMimeRowResult> {
        mime::upsert_mime_row(ctx, request).await
    }

    /// Remove a row from the mime registry.
    ///
    /// A key nothing has a row for is a miss: the caller named a row, and
    /// there was none to take out.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn delete_mime_row(
        &self,
        #[graphql(desc = "The row to remove.")] request: DeleteMimeRowRequest,
    ) -> async_graphql::Result<DeleteMimeRowResult> {
        mime::delete_mime_row(request).await
    }

    /// Change the machine's config.
    ///
    /// A partial edit in four parts: `set` names the settings to change,
    /// `clear` the ones to take back to nothing, `providers` the per-provider
    /// settings, and the two gateway lists what to add and what to remove.
    /// What none of them mentions is left alone. Every refusal happens before
    /// anything is written, so a request that is going to fail leaves the file
    /// as it was.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn update_config(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "What to change.")] request: UpdateConfigRequest,
    ) -> async_graphql::Result<UpdateConfigResult> {
        config::update_config(ctx, request).await
    }

    /// Write a Rhai script.
    ///
    /// Remote code execution by construction, like adding an MCP server: what is
    /// written here is what a run then executes. The answer says whether it
    /// compiles, so an editor does not have to save and wait for a run to fail.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn upsert_script(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The script to write, and what to put in it.")]
        request: UpsertScriptRequest,
    ) -> async_graphql::Result<UpsertScriptResult> {
        scripts::upsert_script(ctx, request).await
    }

    /// Remove a script.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn delete_script(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The script to remove.")] request: DeleteScriptRequest,
    ) -> async_graphql::Result<DeleteScriptResult> {
        scripts::delete_script(ctx, request).await
    }

    /// Run the diagnostics that reach the network.
    ///
    /// The plain `doctor` field answers from the config alone. This one asks a
    /// provider whether a key works and the daemon whether it is there, which
    /// costs a few seconds and is why it is a mutation rather than a field: it is
    /// an act with a cost, and one runs at a time.
    ///
    /// The one mutation with no request, because there is nothing to say: the
    /// checks are the checks.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn check_machine(&self, ctx: &Context<'_>) -> async_graphql::Result<CheckMachineResult> {
        system::check_machine(ctx).await
    }

    /// Make one directory, so a picker can offer "New Folder" rather than one
    /// that refuses.
    ///
    /// The three refusals are told apart on purpose: a path outside
    /// `--workdir-root`, a parent that is not there, and a name already taken are
    /// three different things to show somebody.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn create_directory(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Where to make it, and what to call it.")] request: CreateDirectoryRequest,
    ) -> async_graphql::Result<CreateDirectoryResult> {
        system::create_directory(ctx, request).await
    }

    /// Start a self-update, and hand back the job.
    ///
    /// Answers before the work is done, because the work is a download and an
    /// install: a request held open for a package manager is a console showing a
    /// spinner it made up. Poll `updateJob(id:)`, or watch the live frames. One
    /// update at a time: two package-manager upgrades of the same binary racing
    /// each other is not a state worth debugging.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn start_update(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which steps to run. Omitted means all of them.")]
        request: StartUpdateRequest,
    ) -> async_graphql::Result<StartUpdateResult> {
        system::start_update(ctx, request).await
    }

    /// Sign in to a subscription provider.
    ///
    /// Answers as soon as there is a URL to go to, because what happens after
    /// that is the person's business: they open it, approve, and the flow lands
    /// the grant. Read `providers` to see whether it did.
    ///
    /// The browser has to be on the serving host. The flow listens on a loopback
    /// port there, so a browser anywhere else cannot complete it, and one sign-in
    /// runs at a time because a second could not bind that port.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn sign_in_provider(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The provider to sign in to.")] request: SignInProviderRequest,
    ) -> async_graphql::Result<SignInProviderResult> {
        providers::sign_in_provider(ctx, request).await
    }

    /// Forget a provider's stored sign-in.
    ///
    /// The config is untouched: signing out is not turning the provider off, and
    /// doing both would surprise anybody who meant to sign in again.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn sign_out_provider(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The provider to forget.")] request: SignOutProviderRequest,
    ) -> async_graphql::Result<SignOutProviderResult> {
        providers::sign_out_provider(ctx, request).await
    }

    /// Ask a provider whether the stored sign-in works.
    ///
    /// It asks the account rather than reading a table, so a green answer means
    /// the subscription really did agree, and the models are what that account may
    /// use. That costs a request, which is why this is a mutation.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn check_provider(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The provider to ask.")] request: CheckProviderRequest,
    ) -> async_graphql::Result<CheckProviderResult> {
        providers::check_provider(ctx, request).await
    }

    /// Connect to an MCP server and list what it advertises.
    ///
    /// The only honest answer to "does this server work": a config that parses
    /// proves nothing about a program that will not start.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn check_mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server to connect to.")] request: CheckMcpServerRequest,
    ) -> async_graphql::Result<CheckMcpServerResult> {
        mcp::check_mcp_server(ctx, request).await
    }

    /// Sign in to an MCP server that wants OAuth.
    ///
    /// `NOT_REQUIRED` is a success, not a failure: the question was whether a
    /// sign-in was needed, and the answer is no. Opens a browser on the serving
    /// host, like the provider sign-in.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn sign_in_mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server to sign in to.")] request: SignInMcpServerRequest,
    ) -> async_graphql::Result<SignInMcpServerResult> {
        mcp::sign_in_mcp_server(ctx, request).await
    }

    /// Ask an OpenAI-compatible endpoint what models it serves.
    ///
    /// Makes this host open a connection to an address the caller names, which is
    /// the same act as checking an MCP server, and it exists to precede writing a
    /// gateway for it: a person picks a default from what the endpoint really
    /// serves rather than typing a model id and hoping.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn check_endpoint(
        &self,
        #[graphql(desc = "Where to look, and what to send.")] request: CheckEndpointRequest,
    ) -> async_graphql::Result<CheckEndpointResult> {
        providers::check_endpoint(request).await
    }

    /// Write one yolo profile.
    ///
    /// The whole profile, because a profile is a grant of permissions: an edit
    /// that left half of a previous list behind would describe a set of rules
    /// nobody wrote. Only that one table of `yolo.toml` is touched, so comments
    /// and formatting around it survive; the comments inside the table being
    /// written do not.
    ///
    /// The whole file is checked before anything is written, so a save that
    /// would leave the set unloadable is refused rather than discovered at the
    /// next spawn.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn upsert_yolo_profile(
        &self,
        #[graphql(desc = "The profile to write.")] request: UpsertYoloProfileRequest,
    ) -> async_graphql::Result<UpsertYoloProfileResult> {
        yolo::upsert_yolo_profile(request).await
    }

    /// Remove one yolo profile.
    ///
    /// A name the file has no table for is a miss: the caller named a profile,
    /// and there was none to take out.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn delete_yolo_profile(
        &self,
        #[graphql(desc = "The profile to remove.")] request: DeleteYoloProfileRequest,
    ) -> async_graphql::Result<DeleteYoloProfileResult> {
        yolo::delete_yolo_profile(request).await
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
