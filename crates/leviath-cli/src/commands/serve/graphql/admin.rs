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

use super::super::types::AppState;
use super::config_input::ConfigInput;

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
        #[graphql(desc = "Headers sent with every request to an HTTP server. An \
                    `Authorization` header here is a credential, so the server \
                    needs no separate sign-in.")]
        headers: Option<Vec<super::config_input::EnvEntryInput>>,
    ) -> async_graphql::Result<bool> {
        super::super::mcp::install_server(
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
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn remove_mcp_server(
        &self,
        #[graphql(desc = "The server to remove.")] name: String,
    ) -> async_graphql::Result<bool> {
        super::super::mcp::uninstall_server(&name).gql()?;
        Ok(true)
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
        let tokens = match row.tokens {
            None => None,
            Some(rates) => Some(rates.into_spec().gql()?),
        };
        let written = super::super::mime::write_edit(
            &row.mime_type,
            crate::commands::mime_rows::RowEdit {
                family: row.family,
                text: row.is_text,
                tokens,
                extensions: row.extensions,
                magic: row.magic,
                stand_in: row.stand_in,
                check: row.check,
            },
        )
        .gql()?;
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
    ) -> async_graphql::Result<super::types::machine::Config> {
        let state = ctx.data_unchecked::<AppState>();
        let written = super::super::core::config::write(input.into_request()).gql()?;
        // The models a settings page asks for next are the new config's, so the
        // catalogue starts on them now rather than when that request arrives.
        state
            .caches
            .model_catalog
            .request_refresh(state.current_config(), true);
        Ok(super::query::config_of(
            &written,
            &state.limits.request_limits,
            &state.config.health(),
            // True by construction: this mutation is behind the guard.
            true,
        ))
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
        let state = ctx.data_unchecked::<AppState>();
        let written = super::super::scripts::write_one(
            &state.current_config(),
            &kind,
            &name,
            blueprint.as_deref(),
            &content,
        )
        .gql()?;
        Ok(ScriptWritten {
            path: written.path,
            compiles: written.compiles,
            error: written.error,
        })
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
        let state = ctx.data_unchecked::<AppState>();
        super::super::scripts::remove_one(
            &state.current_config(),
            &kind,
            &name,
            blueprint.as_deref(),
        )
        .gql()?;
        Ok(true)
    }

    /// Run the diagnostics that reach the network.
    ///
    /// The plain `doctor` field answers from the config alone. This one asks a
    /// provider whether a key works and the daemon whether it is there, which
    /// costs a few seconds and is why it is a mutation rather than a field: it is
    /// an act with a cost, and one runs at a time.
    #[graphql(visible = "admin_visible", guard = "AdminGuard")]
    async fn run_doctor_live(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<super::types::machine::DoctorReport> {
        let state = ctx.data_unchecked::<AppState>();
        let checks = super::super::doctor::live_checks(state).await.gql()?;
        Ok(super::query::doctor_report(checks))
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
        let state = ctx.data_unchecked::<AppState>();
        let made = super::super::fs::made(state, &path, &name).gql()?;
        Ok(MadeDirectory {
            path: made.path,
            parent: made.parent,
        })
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
    ) -> async_graphql::Result<super::types::update::UpdateJob> {
        let state = ctx.data_unchecked::<AppState>();
        // The REST route spells this part of the plan `agents`, and the record
        // the job writes carries that word, so the field keeps it while the
        // argument reads in the vocabulary the rest of this schema uses.
        let request = super::super::update_job::ApplyRequest {
            binary,
            agents: blueprints,
            keys,
            migrations,
        };
        // The record the registry wrote, rather than an id read back from it:
        // what a client sees now is the same record `updateJob` will answer
        // with in a moment, and there is no absent case to invent an answer for.
        let job = state
            .update_jobs
            .spawn(request, &state.event_tx)
            .map_err(|running| ServeError::Conflict(format!("update {running} is already running")))
            .gql()?;
        Ok(super::types::update::UpdateJob::from(job))
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
        let state = ctx.data_unchecked::<AppState>();
        let name = super::super::providers::canonical(&provider).gql()?;
        let started = super::super::providers::sign_in_started(state, name)
            .await
            .gql()?;
        Ok(SignInStarted {
            provider: started.provider,
            authorize_url: started.authorize_url,
            already_waiting: started.already_waiting,
        })
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
        let state = ctx.data_unchecked::<AppState>();
        let name = super::super::providers::canonical(&provider).gql()?;
        super::super::providers::signed_out(state, name)
            .await
            .gql()?;
        Ok(true)
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
        let state = ctx.data_unchecked::<AppState>();
        let name = super::super::providers::canonical(&provider).gql()?;
        super::super::providers::checked(state, name).await.gql()
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
        let state = ctx.data_unchecked::<AppState>();
        super::super::mcp::tools_of(state, &name).await.gql()
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
        let state = ctx.data_unchecked::<AppState>();
        let status = super::super::mcp::signed_in(state, &name).await.gql()?;
        Ok(McpLoginStatus::from(status))
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
        #[graphql(desc = "Extra headers the request carries.")] headers: Option<
            Vec<super::config_input::EnvEntryInput>,
        >,
    ) -> async_graphql::Result<Vec<String>> {
        super::super::config::probed(
            super::super::config_types::ProbeModelsReq {
                base_url,
                api_key,
                headers: headers.map(|headers| {
                    headers
                        .into_iter()
                        .map(|entry| (entry.name, entry.value))
                        .collect()
                }),
            },
            &leviath_providers::provider::build_http_client,
        )
        .await
        .gql()
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
    ) -> async_graphql::Result<super::types::machine::YoloProfiles> {
        super::super::yolo::write_profiles(&text).gql()?;
        Ok(super::query::yolo_profiles())
    }
}

/// One row of the mime registry, as a write sends it.
#[derive(async_graphql::InputObject)]
pub(crate) struct MimeRowInput {
    /// The type or pattern this row covers: `image/png`, or `image/*`.
    pub(crate) mime_type: String,
    /// The family providers key their encoders on.
    pub(crate) family: Option<String>,
    /// Whether the bytes are text, and so may travel inline.
    pub(crate) is_text: Option<bool>,
    /// Extensions that imply this type, without the dot.
    pub(crate) extensions: Option<Vec<String>>,
    /// A hex prefix that identifies the bytes.
    pub(crate) magic: Option<String>,
    /// What a consumer that cannot take the type sees in the part's place.
    pub(crate) stand_in: Option<String>,
    /// A script the bytes must pass to be stored as this type. An empty string
    /// lifts a check a broader row put on the type.
    pub(crate) check: Option<String>,
    /// How the tokens are counted.
    pub(crate) tokens: Option<MimeTokensInput>,
}

/// How the tokens of a mime type are counted. Name exactly one rate.
#[derive(async_graphql::InputObject)]
pub(crate) struct MimeTokensInput {
    /// Tokens per byte of the stored file.
    pub(crate) per_byte: Option<f64>,
    /// Pixels one token buys. Pair it with `max`.
    pub(crate) per_pixel: Option<i32>,
    /// The most one part may cost, and the answer when the dimensions are
    /// unknown. Only with `perPixel`.
    pub(crate) max: Option<i32>,
    /// Tokens per second of audio or video.
    pub(crate) per_second: Option<i32>,
    /// Tokens per page of a document.
    pub(crate) per_page: Option<i32>,
    /// A flat charge, whatever the size.
    pub(crate) fixed: Option<i32>,
}

impl MimeTokensInput {
    /// The rule these rates describe, or why they describe none.
    ///
    /// Through the same reader the REST route uses, so "exactly one rate" means
    /// the same thing on both surfaces.
    fn into_spec(self) -> Result<crate::commands::mime_rows::TokenSpec, ServeError> {
        super::super::mime::TokenRuleReq {
            per_byte: self.per_byte,
            per_pixel: self.per_pixel.map(i64::from),
            per_second: self.per_second.map(i64::from),
            per_page: self.per_page.map(i64::from),
            fixed: self.fixed.map(i64::from),
            max: self.max.map(i64::from),
        }
        .into_spec()
        .map_err(ServeError::BadRequest)
    }
}

/// What writing a script did.
#[derive(Debug, SimpleObject)]
pub(crate) struct ScriptWritten {
    /// Where it was written.
    pub(crate) path: String,
    /// Whether it compiles. A script that does not is still written: an editor
    /// saves work in progress, and the run is what refuses to use it.
    pub(crate) compiles: bool,
    /// Why it does not compile, when it does not.
    pub(crate) error: Option<String>,
}

/// A directory that was made.
#[derive(Debug, SimpleObject)]
pub(crate) struct MadeDirectory {
    /// The new directory.
    pub(crate) path: String,
    /// The directory it was made in.
    pub(crate) parent: String,
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;

/// A provider sign-in that is waiting for the person to finish it.
#[derive(Debug, SimpleObject)]
pub(crate) struct SignInStarted {
    /// The provider, by its canonical name.
    pub(crate) provider: String,
    /// Where the person has to go, on the serving host.
    pub(crate) authorize_url: String,
    /// Whether this is the sign-in somebody already started rather than a new
    /// one. The URL is the same either way, which is what a client needs.
    pub(crate) already_waiting: bool,
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
impl From<super::super::mcp::LoginStatus> for McpLoginStatus {
    /// Its own impl rather than a match inside the resolver: reaching that
    /// resolver means completing an OAuth handshake against a real server, and
    /// the mapping is worth checking without one.
    fn from(status: super::super::mcp::LoginStatus) -> Self {
        match status {
            super::super::mcp::LoginStatus::Authenticated => Self::Authenticated,
            super::super::mcp::LoginStatus::NotRequired => Self::NotRequired,
        }
    }
}
