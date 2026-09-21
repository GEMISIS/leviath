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
//! registry, [`scripts`] for a registered script, [`yolo`] for the profiles
//! file, [`providers`] for subscription sign-in and endpoint probing, and
//! [`system`] for the acts that touch the machine itself: updates, live
//! diagnostics, and making a directory. `AdminMutation` stays one
//! `#[Object] impl` with one field per method, for the same reason `Query`
//! does: this schema's field order does not sort into those seven groups, so
//! `MergedObject` cannot reproduce it, and each method here is a one-line
//! delegation into its group's module instead.

use async_graphql::{Context, Guard, Object};

use super::config_input::{ConfigInput, EnvEntryInput};
use super::error::graphql_error;
use super::types::machine::{Config, DoctorReport, YoloProfiles};
use super::types::update::UpdateJob;

use super::super::core::error::ServeError;

pub(crate) mod config;
pub(crate) mod mcp;
pub(crate) mod mime;
pub(crate) mod providers;
pub(crate) mod scripts;
pub(crate) mod system;
pub(crate) mod yolo;

use mcp::McpLoginStatus;
use mime::{MimeRowInput, MimeRowWritten};
use providers::SignInStarted;
use scripts::ScriptWritten;
use system::MadeDirectory;

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
    async fn add_mcp_server(
        &self,
        #[graphql(desc = "Unique server name.")] name: String,
        #[graphql(desc = "The command, for a stdio server.")] command: Option<String>,
        #[graphql(desc = "The URL, for an HTTP server.")] url: Option<String>,
        #[graphql(desc = "Arguments for a stdio server.")] args: Option<Vec<String>>,
        #[graphql(desc = "Headers sent with every request to an HTTP server. An \
                    `Authorization` header here is a credential, so the server \
                    needs no separate sign-in.")]
        headers: Option<Vec<EnvEntryInput>>,
    ) -> async_graphql::Result<bool> {
        mcp::add_mcp_server(name, command, url, args, headers).await
    }

    /// Remove an MCP server from the config.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn remove_mcp_server(
        &self,
        #[graphql(desc = "The server to remove.")] name: String,
    ) -> async_graphql::Result<bool> {
        mcp::remove_mcp_server(name).await
    }

    /// Add or update one row of the mime registry.
    ///
    /// Every field but the key is optional, because a row says only what it
    /// changes: what a field leaves out stays as whatever broader row already
    /// covers the type.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn put_mime_row(
        &self,
        #[graphql(desc = "The row to write.")] row: MimeRowInput,
    ) -> async_graphql::Result<MimeRowWritten> {
        mime::put_mime_row(row).await
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
        mime::delete_mime_row(mime_type).await
    }

    /// Change the machine's config.
    ///
    /// A partial edit: a field left out leaves the setting alone, `null` clears
    /// it, and a value sets it. An empty string is refused rather than read as a
    /// clear, because a form that posts its empty box should be told rather than
    /// obeyed. Every refusal happens before anything is written, so a request
    /// that is going to fail leaves the file as it was.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn update_config(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "What to change.")] input: ConfigInput,
    ) -> async_graphql::Result<Config> {
        config::update_config(ctx, input).await
    }

    /// Write a Rhai script.
    ///
    /// Remote code execution by construction, like adding an MCP server: what is
    /// written here is what a run then executes. The answer says whether it
    /// compiles, so an editor does not have to save and wait for a run to fail.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn put_script(
        &self,
        ctx: &Context<'_>,
        #[graphql(
            desc = "Which registry: tool, region_hook, stage_hook, output_validator, \
                          mime_check or provider."
        )]
        kind: String,
        #[graphql(desc = "Its name, unique within that kind.")] name: String,
        #[graphql(desc = "The script's source.")] content: String,
        #[graphql(
            desc = "The blueprint whose directory it belongs to, for a blueprint-scoped script."
        )]
        blueprint: Option<String>,
    ) -> async_graphql::Result<ScriptWritten> {
        scripts::put_script(ctx, kind, name, content, blueprint).await
    }

    /// Remove a script.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn delete_script(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which registry it belongs to.")] kind: String,
        #[graphql(desc = "The script to remove.")] name: String,
        #[graphql(desc = "The blueprint whose directory it is in.")] blueprint: Option<String>,
    ) -> async_graphql::Result<bool> {
        scripts::delete_script(ctx, kind, name, blueprint).await
    }

    /// Run the diagnostics that reach the network.
    ///
    /// The plain `doctor` field answers from the config alone. This one asks a
    /// provider whether a key works and the daemon whether it is there, which
    /// costs a few seconds and is why it is a mutation rather than a field: it is
    /// an act with a cost, and one runs at a time.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn run_doctor_live(&self, ctx: &Context<'_>) -> async_graphql::Result<DoctorReport> {
        system::run_doctor_live(ctx).await
    }

    /// Make one directory, so a picker can offer "New Folder" rather than one
    /// that refuses.
    ///
    /// The three refusals are told apart on purpose: a path outside
    /// `--workdir-root`, a parent that is not there, and a name already taken are
    /// three different things to show somebody.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn make_directory(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The existing directory to make it in, absolute.")] path: String,
        #[graphql(desc = "One directory name, not a path.")] name: String,
    ) -> async_graphql::Result<MadeDirectory> {
        system::make_directory(ctx, path, name).await
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
        #[graphql(desc = "Upgrade the binary.", default = true)] binary: bool,
        #[graphql(desc = "Install the bundled blueprints.", default = true)] blueprints: bool,
        #[graphql(
            desc = "Respell keys that changed name in your own blueprints.",
            default = true
        )]
        keys: bool,
        #[graphql(desc = "Apply the config migrations.", default = true)] migrations: bool,
    ) -> async_graphql::Result<UpdateJob> {
        system::start_update(ctx, binary, blueprints, keys, migrations).await
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
    async fn provider_sign_in(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The provider, by name.")] provider: String,
    ) -> async_graphql::Result<SignInStarted> {
        providers::provider_sign_in(ctx, provider).await
    }

    /// Forget a provider's stored sign-in.
    ///
    /// The config is untouched: signing out is not turning the provider off, and
    /// doing both would surprise anybody who meant to sign in again.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn provider_sign_out(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The provider, by name.")] provider: String,
    ) -> async_graphql::Result<bool> {
        providers::provider_sign_out(ctx, provider).await
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
        #[graphql(desc = "The provider, by name.")] provider: String,
    ) -> async_graphql::Result<Vec<String>> {
        providers::check_provider(ctx, provider).await
    }

    /// Connect to an MCP server and list what it advertises.
    ///
    /// The only honest answer to "does this server work": a config that parses
    /// proves nothing about a program that will not start.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn test_mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server, by name.")] name: String,
    ) -> async_graphql::Result<Vec<String>> {
        mcp::test_mcp_server(ctx, name).await
    }

    /// Sign in to an MCP server that wants OAuth.
    ///
    /// `NOT_REQUIRED` is a success, not a failure: the question was whether a
    /// sign-in was needed, and the answer is no. Opens a browser on the serving
    /// host, like the provider sign-in.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn login_mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server, by name.")] name: String,
    ) -> async_graphql::Result<McpLoginStatus> {
        mcp::login_mcp_server(ctx, name).await
    }

    /// Ask an OpenAI-compatible endpoint what models it serves.
    ///
    /// Makes this host open a connection to an address the caller names, which is
    /// the same act as testing an MCP server, and it exists to precede writing a
    /// gateway for it: a person picks a default from what the endpoint really
    /// serves rather than typing a model id and hoping.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn probe_models(
        &self,
        #[graphql(desc = "Where the endpoint is.")] base_url: String,
        #[graphql(desc = "Its API key, when it wants one. Used for this one call and \
                    dropped: never written to the config, and this server logs no \
                    request body, so it reaches nothing on disk.")]
        api_key: Option<String>,
        #[graphql(desc = "Extra headers the request carries.")] headers: Option<Vec<EnvEntryInput>>,
    ) -> async_graphql::Result<Vec<String>> {
        providers::probe_models(base_url, api_key, headers).await
    }

    /// Replace the yolo profiles file.
    ///
    /// The whole file, because the file is the unit: `--yolo=<name>` names a
    /// profile inside it and the profiles refer to each other, so writing one at a
    /// time would let a save leave the set inconsistent. Parsed before it is
    /// written, so a file that would not load is refused rather than saved and
    /// discovered at the next spawn.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn put_yolo_profiles(
        &self,
        #[graphql(desc = "The whole file, as TOML.")] text: String,
    ) -> async_graphql::Result<YoloProfiles> {
        yolo::put_yolo_profiles(text).await
    }
}

// Re-exported for the tests below, which build these inputs and read this
// status directly rather than through a query document.
#[cfg(test)]
use mime::MimeTokensInput;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
