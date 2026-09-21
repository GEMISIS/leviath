//! The read side of the schema.
//!
//! Every field here turns its arguments into a service-layer call and its
//! answer into GraphQL objects. No field reaches for a REST route, and none
//! re-implements a filter: `runs` builds the same [`RunSelection`] the REST
//! listing builds, so the two cannot disagree about which runs match.
//!
//! One file per concern: [`blueprints`] for the catalogue installed on this
//! machine, [`runs`] for the run listing and the approval inbox, [`machine`]
//! for what this server is configured to do, [`catalog`] for the models,
//! providers and tools a run can use, [`checks`] for the pure validations,
//! and [`jobs`] for the background jobs a client polls. `Query` itself stays
//! one `#[Object] impl` with one field per method: `async-graphql`'s
//! `MergedObject` orders a merged type's fields by member then declaration
//! order, and this schema's field order does not sort into those six groups,
//! so each method here is a one-line delegation into its group's module
//! instead.
//!
//! [`RunSelection`]: super::run_filter::RunSelection

use async_graphql::{Context, Object};

use blueprints::BlueprintConnection;
use runs::OpenInteraction;

use super::blueprint_filter::BlueprintFilter;
use super::checks::{KeyVerdict, ScriptVerdict, ValidationReport, YoloDecision, YoloTestInput};
use super::connection::RunConnection;
use super::inputs::BlueprintInput;
use super::node::Node;
use super::run_filter::RunFilter;
use super::scalars::Cursor;
use super::types::catalog::{Model, Provider, ToolInventory};
use super::types::machine::{
    Config, Directory, DoctorReport, JournalHealth, McpServer, MimeRow, Script, YoloProfiles,
};
use super::types::update::{DaemonStatus, UpdateInfo, UpdateJob};

pub(crate) mod blueprints;
pub(crate) mod catalog;
pub(crate) mod checks;
pub(crate) mod jobs;
pub(crate) mod machine;
pub(crate) mod runs;

/// An export job, as a client polls it. Re-exported so `node` and the
/// `bulkExportRuns` mutation answer with the same type this field does.
pub(crate) use jobs::BulkExport;

/// The config as this schema describes it, with every secret left out.
///
/// Shared with the write side, so a config read and the answer to a config write
/// are the same shape rather than two that drifted.
pub(crate) use machine::config_of;
/// One diagnostics run as this schema describes it.
///
/// Shared by the offline field and the live mutation: they run different checks
/// and answer with the same shape, which is what lets a client render one view.
pub(crate) use machine::doctor_report;
/// Re-exported for the tests below, which check the mapping from the config
/// file's own words to this schema's enum directly rather than through a
/// query.
#[cfg(test)]
use machine::waiver_word;
/// The yolo profiles as this schema describes them.
///
/// Shared by the field and the write, so "what is there now" is one shape
/// whichever asked.
pub(crate) use machine::yolo_profiles;

/// The resolver state behind the `Query` type.
pub(crate) struct Query;

/// The whole read side: runs and their history, the blueprints and tools
/// installed on this machine, and what the daemon itself is doing.
///
/// Start from `runs` to find work, and from `node(id:)` when you already hold
/// an id: anything this schema gives an id to comes back from there, whatever
/// type it is. Reading never changes a run, so a query is safe to repeat and
/// safe to poll.
#[Object]
impl Query {
    /// The blueprints installed on this machine, by name.
    ///
    /// This is the live definition, not what any run executed: for that, read
    /// `blueprint` on the run, which answers from the run's own snapshot. The
    /// digests tell you whether the two are the same bytes.
    ///
    /// `filter.names` fetches blueprints by name. A name that is not installed
    /// lands in `missing` rather than failing the request.
    ///
    /// Keyset-paged on the name, which is the order the catalogue is read in.
    /// A cursor names where you got to, so a blueprint installed or removed
    /// mid-walk cannot make a page skip or repeat one.
    async fn blueprints(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which blueprints to list. Omitted means all of them.")] filter: Option<
            BlueprintFilter,
        >,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page's pageInfo.")] after: Option<Cursor>,
    ) -> async_graphql::Result<BlueprintConnection> {
        blueprints::blueprints(ctx, filter, first, after).await
    }

    /// How this server is configured, with every secret left out.
    ///
    /// Read `capabilities` before choosing a code path. A 404 also means "no
    /// such run", so discovering a feature by being refused costs a round trip
    /// and tells you less.
    async fn config(&self, ctx: &Context<'_>) -> Config {
        machine::config(ctx).await
    }

    /// Environment and configuration diagnostics.
    ///
    /// A failing check is `ok: false` inside a healthy answer, never an error:
    /// the request to run the checks succeeded, and what they found is the
    /// answer.
    async fn doctor(&self) -> DoctorReport {
        machine::doctor().await
    }

    /// The MCP servers this machine has configured.
    async fn mcp_servers(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<McpServer>> {
        machine::mcp_servers(ctx).await
    }

    /// The yolo profiles, and the file they are read from.
    async fn yolo_profiles(&self) -> YoloProfiles {
        machine::yolo_profiles()
    }

    /// The operator's mime registry, before any blueprint's own rows.
    async fn mime(&self, ctx: &Context<'_>) -> Vec<MimeRow> {
        machine::mime(ctx).await
    }

    /// The scripts this machine has registered.
    async fn scripts(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only this blueprint's own scripts, plus the global ones.")]
        blueprint: Option<BlueprintInput>,
    ) -> async_graphql::Result<Vec<Script>> {
        machine::scripts(ctx, blueprint).await
    }

    /// The directories under a path, for a file picker.
    ///
    /// Confined to `--workdir-root` when the operator set one, which is also
    /// why `parent` is null at that fence rather than leading above it.
    async fn directories(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The directory to list. Omitted means this server's own.")] path: Option<
            String,
        >,
        #[graphql(desc = "Include hidden directories.", default = false)] hidden: bool,
    ) -> async_graphql::Result<Directory> {
        machine::directories(ctx, path, hidden).await
    }

    /// Every model this machine can route to.
    ///
    /// Answered from the catalogue this server keeps, so it costs no provider
    /// call. Two providers can serve the same model id and bill to different
    /// places, so `provider` is part of each answer rather than something a
    /// client infers.
    async fn models(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only this provider's models.")] provider: Option<String>,
        #[graphql(
            desc = "Refresh this server's catalogue from the providers before answering, \
                    instead of answering from what it already holds. Slower, and it \
                    changes nothing a later request would not see anyway.",
            default = false
        )]
        refresh: bool,
    ) -> Vec<Model> {
        catalog::models(ctx, provider, refresh).await
    }

    /// The providers this machine can reach, configured or not.
    ///
    /// `enabled` and `signedIn` are different questions with different
    /// answers: a provider can be turned on with no credential stored, and a
    /// credential can outlive the config entry that used it.
    async fn providers(&self, ctx: &Context<'_>) -> Vec<Provider> {
        catalog::providers(ctx).await
    }

    /// The tools a run on this machine can call.
    ///
    /// Scoped to one blueprint's own directory when `blueprint` names one,
    /// which is what an editor offering an `available_tools` list wants.
    async fn tools(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Scope to this blueprint's own tools directory.")] blueprint: Option<
            BlueprintInput,
        >,
    ) -> async_graphql::Result<ToolInventory> {
        catalog::tools(ctx, blueprint).await
    }

    /// Who is on the other end of the control socket.
    ///
    /// A read this server answers from what it already knows, so it works while
    /// the daemon is down: that is the point of asking. `connected` false does
    /// not mean requests fail, it means the live frames have stopped.
    async fn daemon(&self, ctx: &Context<'_>) -> DaemonStatus {
        machine::daemon(ctx).await
    }

    /// Whether the daemon is still recording what its runs do.
    ///
    /// Null when the daemon cannot be reached, because this is the daemon's own
    /// reading and no other copy of it exists - `daemon.reachable` says whether
    /// that is why. Everything else about a run is read from disk and keeps
    /// working while the daemon is down; this does not.
    ///
    /// Worth asking on any page that shows runs as healthy. A daemon whose
    /// journal is refusing writes serves every field here exactly as it did
    /// before, and a run whose journal record cannot be written is failed rather
    /// than carried on.
    async fn journal(&self, ctx: &Context<'_>) -> Option<JournalHealth> {
        machine::journal(ctx).await
    }

    /// What an update would do, and whether there is anything newer to get.
    ///
    /// Planning never reaches the network. The "is there anything newer" half is
    /// whatever the last check found, and asking starts another one for whoever
    /// asks next rather than waiting on one here, so this is cheap enough for a
    /// page to ask every time it opens.
    async fn update(&self, ctx: &Context<'_>) -> UpdateInfo {
        machine::update(ctx).await
    }

    /// Anything with a globally unique id, from that id alone.
    ///
    /// For a client that holds an id and no type: a webhook payload, a cache
    /// key, a link somebody pasted. Ask for the fields on `Node` and narrow
    /// with `... on Run { }` for the rest.
    ///
    /// How an id routes, in order. An id tagged `mcpServer:`, `yoloProfile:` or
    /// `script:` names that kind of thing; a tag this server does not know
    /// answers null. An id carrying an `@` is a blueprint revision. Anything
    /// else is a minted id, and the two job registries are asked before the run
    /// store, which is the only one of the three that reads a file.
    ///
    /// Null rather than an error for an id that names nothing: a deleted run, an
    /// expired export and a typo are the same answer, and all three mean the
    /// thing is not here. A read that could not answer the question at all, such
    /// as a config file that will not parse, fails the way the listing it would
    /// have come from fails.
    ///
    /// `Model` is not a `Node`, because a model id is the provider's own and two
    /// providers can serve the same one; read `models` and key on provider and
    /// id together.
    async fn node(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The id, as whatever holds it spelled it.")] id: async_graphql::ID,
    ) -> async_graphql::Result<Option<Node>> {
        runs::node(ctx, id).await
    }

    /// One update run, by id.
    ///
    /// Null when no job carries that id. The last few runs are kept, so an
    /// operator reading back after the fact finds the job rather than nothing.
    async fn update_job(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The job's id.")] id: String,
    ) -> Option<UpdateJob> {
        jobs::update_job(ctx, id).await
    }

    /// Poll an export this server started.
    ///
    /// Null when no export carries that id: it was never started, or it has
    /// expired. An export's file is kept for an hour, and its record goes with
    /// the file, so neither outlives the other.
    async fn bulk_export(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The export job's id.")] id: String,
    ) -> Option<BulkExport> {
        jobs::bulk_export(ctx, id).await
    }

    /// Every open ask across every run: the approval inbox.
    ///
    /// The daemon holds these in memory, so this is one read rather than a walk
    /// of the run store. Each entry names the run it is parked on, which is
    /// what a client needs to show the row it belongs to.
    async fn open_interactions(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Vec<OpenInteraction>> {
        runs::open_interactions(ctx).await
    }

    /// Keyset-paged run listing.
    ///
    /// `filter.ids` fetches exact runs, which is also how a client reads one
    /// run: `runs(filter: { ids: ["..."] })`. An id that names nothing lands
    /// in `missing` rather than failing the request, so one dead id in a batch
    /// of fifty does not cost the other forty-nine.
    async fn runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which runs to list. Omitted means all of them.")] filter: Option<
            RunFilter,
        >,
        #[graphql(
            desc = "Page size; capped by the server's page-size limit.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page's pageInfo.")] after: Option<Cursor>,
    ) -> async_graphql::Result<RunConnection> {
        runs::runs(ctx, filter, first, after).await
    }

    // ─── The pure checks ──────────────────────────────────────────────────
    //
    // Text in, verdict out: nothing is written, nothing is dialled, nothing is
    // run. Each one usually precedes a write, which is where it sits in a form,
    // not what it does, so each is a field rather than a mutation.

    /// Check a manifest without installing it.
    ///
    /// A failing check is a report, not an error: the request to validate
    /// succeeded, and what it found is the answer.
    async fn validate_blueprint(
        &self,
        #[graphql(desc = "The manifest text to check.")] content: String,
        #[graphql(
            desc = "Check the text as this installed blueprint, so its own scripts resolve."
        )]
        name: Option<String>,
    ) -> async_graphql::Result<ValidationReport> {
        checks::validate_blueprint(content, name).await
    }

    /// Whether a provider key looks like one of that provider's.
    ///
    /// Format only: nothing is dialled and nothing is written, which is what
    /// makes it safe to run on every keystroke of a form. `checkProvider` is
    /// the one that asks the account.
    async fn validate_config_key(
        &self,
        #[graphql(desc = "The provider the key is for.")] provider: String,
        #[graphql(desc = "The key to look at. Never stored, never logged.")] key: String,
        #[graphql(desc = "A gateway's address, checked first when given.")] base_url: Option<
            String,
        >,
    ) -> KeyVerdict {
        checks::validate_config_key(provider, key, base_url).await
    }

    /// Whether a script compiles, without writing it.
    ///
    /// The alternative was saving it and waiting for a run to fail, which is
    /// not much of an improvement on editing the file over SSH. Ungated:
    /// compiling text in memory writes nothing and runs nothing, because every
    /// compiler here stops at the syntax tree.
    async fn validate_script(
        &self,
        #[graphql(desc = "Which registry the script is for.")] kind: String,
        #[graphql(desc = "The source to compile.")] content: String,
        #[graphql(desc = "Hook functions it has to define, for a stage or region hook.")]
        hooks: Option<Vec<String>>,
    ) -> async_graphql::Result<ScriptVerdict> {
        checks::validate_script(kind, content, hooks).await
    }

    /// What one yolo profile would do with one call.
    ///
    /// The same code path `lev yolo test` runs, so the command and the API cannot
    /// disagree about a call. Decides and reports; nothing is run.
    async fn test_yolo_profile(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The call to decide about.")] call: YoloTestInput,
    ) -> async_graphql::Result<YoloDecision> {
        checks::test_yolo_profile(ctx, call).await
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
