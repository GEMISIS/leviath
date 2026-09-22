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
//! [`RunSelection`]: super::super::core::runs::RunSelection

use async_graphql::{Context, ID, Object};

use super::paging::page::weight;

use runs::{RunListingExtras, RunSearchOptions};

use super::checks::{KeyVerdict, ScriptVerdict, ValidationReport};
use super::connection::Connection;
use super::inputs::BlueprintRef;
use super::node::Node;
use super::scalars::{Cursor, Timestamp};
use super::script_ref::{ScriptKind, ScriptRef};
use super::types::blueprint::{Blueprint, BlueprintFilter, BlueprintOrder};
use super::types::catalog::{
    Model, ModelFilter, ModelOrder, Provider, ProviderFilter, ProviderOrder, Tool, ToolFilter,
    ToolGroup, ToolOrder, ToolSkips,
};
use super::types::machine::{
    Config, Directory, DoctorReport, McpServer, McpServerFilter, McpServerOrder, MimeRow,
    MimeRowFilter, MimeRowOrder, Script, ScriptFilter, ScriptOrder, YoloProfile, YoloProfileFilter,
    YoloProfileOrder,
};
use super::types::run::{Run, RunFilter, RunOrder};
use super::types::update::{DaemonStatus, UpdateJob, UpdateJobFilter, UpdateJobOrder, UpdatePlan};

pub(crate) mod blueprints;
pub(crate) mod catalog;
pub(crate) mod checks;
pub(crate) mod jobs;
pub(crate) mod listing;
pub(crate) mod machine;
pub(crate) mod runs;

/// One model, from the node id `model:<provider>/<modelId>` carries. Shared
/// with `node`, so the lookup and the listing cannot disagree about what is
/// there.
pub(crate) use catalog::model_by_id;
/// One provider, from the node id `provider:<name>` carries. Shared with
/// `node`, so the lookup and the listing cannot disagree about what is there.
pub(crate) use catalog::provider_by_id;
/// An export of the run store, as a client polls it. Re-exported so `node` and
/// the export mutation answer with the same type this field does.
pub(crate) use jobs::RunExport;

/// The config as this schema describes it, with every secret left out.
///
/// Shared with the write side, so a config read and the answer to a config write
/// are the same shape rather than two that drifted.
pub(crate) use machine::config_of;
/// One diagnostics run that reached the network.
///
/// Shared with the live mutation: it runs different checks from the offline
/// field and answers with the same shape, which is what lets a client render
/// one view.
pub(crate) use machine::live_doctor_report;
/// One yolo profile as this schema describes it.
///
/// Shared by the field, `node` and the write, so "what is there now" is one
/// shape whichever asked.
pub(crate) use machine::yolo_profile;

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
    /// The blueprints installed on this machine.
    ///
    /// This is the live definition, not what any run executed: for that, read
    /// `blueprint` on the run, which answers from the run's own snapshot. The
    /// digests tell you whether the two are the same bytes.
    ///
    /// Keyset-paged, by name ascending unless `orderBy` says otherwise. A
    /// cursor names where you got to, so a blueprint installed or removed
    /// mid-walk cannot make a page skip or repeat one.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn blueprints(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which blueprints to list. Omitted means all of them.")] filter: Option<
            BlueprintFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means name ascending.")]
        order_by: Option<Vec<BlueprintOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Blueprint>> {
        blueprints::blueprints(ctx, filter, order_by, first, after).await
    }

    /// One installed blueprint, by the name it is installed under.
    ///
    /// Null for a name nothing is installed under. A lookup answers "not
    /// here" rather than failing, so reading several names costs one request
    /// and gives an answer for each.
    async fn blueprint(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The name the blueprint is installed under.")] name: String,
    ) -> Option<Blueprint> {
        blueprints::blueprint(ctx, name).await
    }

    /// How this server is configured, with every secret left out.
    ///
    /// Everything `updateConfig` writes is here under the same name, so a
    /// settings screen renders what it saves. A key is the exception: it reads
    /// back as `hasKey` on its provider.
    ///
    /// Read `server.capabilities` before choosing a code path. A 404 also
    /// means "no such run", so discovering a feature by being refused costs a
    /// round trip and tells you less.
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
    ///
    /// Keyset-paged, by name ascending unless `orderBy` says otherwise, which
    /// is the key a machine holds one server per.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn mcp_servers(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which servers to list. Omitted means all of them.")] filter: Option<
            McpServerFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means name ascending.")]
        order_by: Option<Vec<McpServerOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<McpServer>> {
        machine::mcp_servers(ctx, filter, order_by, first, after).await
    }

    /// One configured MCP server, by the name it is configured under.
    ///
    /// Null for a name this machine has no server for. A config that will not
    /// parse still fails, because that is not the same answer as "no such
    /// server".
    async fn mcp_server(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The server's name in the config.")] name: String,
    ) -> async_graphql::Result<Option<McpServer>> {
        machine::mcp_server(ctx, name).await
    }

    /// The yolo profiles this machine has configured.
    ///
    /// Where the file is and whether it loads is `config.yoloFile`: this is
    /// what the file holds. A file that does not load holds nothing, and the
    /// reason is there rather than here.
    ///
    /// Keyset-paged, by name ascending unless `orderBy` says otherwise, which
    /// is the key the file holds one profile per.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn yolo_profiles(
        &self,
        #[graphql(desc = "Which profiles to list. Omitted means all of them.")] filter: Option<
            YoloProfileFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means name ascending.")]
        order_by: Option<Vec<YoloProfileOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<YoloProfile>> {
        machine::yolo_profile_page(filter, order_by, first, after).await
    }

    /// One yolo profile, by the name `--yolo=<name>` spells.
    ///
    /// Null for a name the file has no table for, which is also what a file
    /// that does not load answers: `config.yoloFile.error` says whether that
    /// is why.
    async fn yolo_profile(
        &self,
        #[graphql(desc = "The profile's name in the file.")] name: String,
    ) -> Option<YoloProfile> {
        machine::yolo_profile(&name)
    }

    /// The operator's mime registry, before any blueprint's own rows.
    ///
    /// Keyset-paged, by mime type ascending unless `orderBy` says otherwise,
    /// which is the order the registry itself is stored in.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn mime_rows(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which rows to list. Omitted means all of them.")] filter: Option<
            MimeRowFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means mime type ascending.")]
        order_by: Option<Vec<MimeRowOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<MimeRow>> {
        machine::mime_rows(ctx, filter, order_by, first, after).await
    }

    /// The scripts every run on this machine can see.
    ///
    /// For one blueprint's own scripts as well, read `scripts` on that
    /// blueprint: the scope changes which directory is walked, so it is a field
    /// there rather than an argument here.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn scripts(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which scripts to list. Omitted means all of them.")] filter: Option<
            ScriptFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means id ascending.")]
        order_by: Option<Vec<ScriptOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Script>> {
        machine::scripts(ctx, filter, order_by, first, after).await
    }

    /// One registered script, by the three things that name it.
    ///
    /// Null for a reference nothing is filed under, which is what a client that
    /// guessed a kind or a blueprint gets rather than an error.
    async fn script(
        &self,
        ctx: &Context<'_>,
        #[graphql(
            name = "ref",
            desc = "Which script: its kind, its name, and whose it is."
        )]
        reference: ScriptRef,
    ) -> async_graphql::Result<Option<Script>> {
        machine::script(ctx, reference).await
    }

    /// The directories under a path, for a file picker.
    ///
    /// Confined to `--workdir-root` when the operator set one, which is also
    /// why `parent` is null at that fence rather than leading above it.
    async fn directory(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The directory to list. Omitted means this server's own.")] path: Option<
            String,
        >,
        #[graphql(desc = "Include hidden directories.", default = false)] include_hidden: bool,
    ) -> async_graphql::Result<Directory> {
        machine::directory(ctx, path, include_hidden).await
    }

    /// Every model this machine can route to.
    ///
    /// Answered from the catalogue this server keeps, so it costs no provider
    /// call and never waits on one. `refreshModels` is the mutation that asks
    /// the providers for a newer one.
    ///
    /// Two providers can serve the same model id and bill to different places,
    /// so the provider is part of each answer rather than something a client
    /// infers: for one provider's models, filter on `providerName`.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn models(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which models to list. Omitted means all of them.")] filter: Option<
            ModelFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means id ascending.")]
        order_by: Option<Vec<ModelOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Model>> {
        catalog::models(ctx, filter, order_by, first, after).await
    }

    /// One model, by the id this schema gives it.
    ///
    /// Null for an id nothing here serves, which covers a model a provider has
    /// retired and one this machine has no provider for.
    async fn model(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The model's id: `model:<provider>/<modelId>`.")] id: ID,
    ) -> Option<Model> {
        catalog::model(ctx, id).await
    }

    /// The providers a person signs in to through a browser, signed in or not.
    ///
    /// Not every provider this machine can talk to. One that takes an API key
    /// is never signed in to, so it is not here; `config.providers` is where
    /// every provider this build knows is listed. This field is the state
    /// behind `signInProvider` and `signOutProvider`, which is why it is the
    /// sign-in ones and only those.
    ///
    /// `enabled` and `signedIn` are different questions with different
    /// answers: a provider can be turned on with no credential stored, and a
    /// credential can outlive the config entry that used it.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn providers(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which providers to list. Omitted means all of them.")] filter: Option<
            ProviderFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means name ascending.")]
        order_by: Option<Vec<ProviderOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Provider>> {
        catalog::providers(ctx, filter, order_by, first, after).await
    }

    /// One browser sign-in provider, by the registry name a blueprint writes.
    ///
    /// Null for a name `providers` does not list, a provider that takes an API
    /// key included.
    async fn provider(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The registry name, such as `openai`.")] name: String,
    ) -> Option<Provider> {
        catalog::provider(ctx, name).await
    }

    /// The tools a run on this machine can call.
    ///
    /// For one blueprint's own tools as well, read `tools` on that blueprint:
    /// the scope changes which directory is walked, so it is a field there
    /// rather than an argument here.
    ///
    /// `skipped` sits beside the page: it is what the same walk of the same
    /// directories found and could not offer, with the reason.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn tools(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which tools to list. Omitted means all of them.")] filter: Option<
            ToolFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means name ascending.")]
        order_by: Option<Vec<ToolOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Tool, ToolSkips>> {
        catalog::tools(ctx, filter, order_by, first, after).await
    }

    /// The group tokens an `available_tools` list may name in place of tool
    /// names.
    ///
    /// A bare list: this build compiles the set in, so it is bounded by the
    /// code rather than by anything on the machine.
    async fn tool_groups(&self) -> Vec<ToolGroup> {
        catalog::tool_groups().await
    }

    /// Who is on the other end of the control socket.
    ///
    /// A read this server answers from what it already knows, so it works while
    /// the daemon is down: that is the point of asking. `reachable` false does
    /// not mean requests fail, it means the live frames have stopped. The one
    /// field here that does need the daemon is `journal`, and it costs a
    /// control call only where it is selected.
    async fn daemon(&self, ctx: &Context<'_>) -> DaemonStatus {
        machine::daemon(ctx).await
    }

    /// What an update would do, and whether there is anything newer to get.
    ///
    /// Planning never reaches the network. The "is there anything newer" half is
    /// whatever the last check found, and asking starts another one for whoever
    /// asks next rather than waiting on one here, so this is cheap enough for a
    /// page to ask every time it opens.
    async fn update_plan(&self, ctx: &Context<'_>) -> UpdatePlan {
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
    async fn node(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The id, as whatever holds it spelled it.")] id: ID,
    ) -> async_graphql::Result<Option<Node>> {
        super::node::resolve(ctx, id.as_str()).await
    }

    /// Several nodes, from their ids alone.
    ///
    /// One entry per id, in the order they were asked about, and null where an
    /// id names nothing. That is what lets a client holding a page of cached
    /// keys line the answers up against what it asked rather than matching on
    /// ids, and it is why one dead id costs nothing but its own slot.
    async fn nodes(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The ids to look up, as whatever holds them spelled them.")] ids: Vec<ID>,
    ) -> async_graphql::Result<Vec<Option<Node>>> {
        super::node::resolve_many(ctx, ids).await
    }

    /// One update run, by id.
    ///
    /// Null when no job carries that id. The last few runs are kept, so an
    /// operator reading back after the fact finds the job rather than nothing.
    async fn update_job(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The job's id.")] id: ID,
    ) -> Option<UpdateJob> {
        jobs::update_job(ctx, id).await
    }

    /// Every update run this server has done.
    ///
    /// Keyset-paged, oldest first unless `orderBy` says otherwise. A job's id
    /// carries the second it started, so ordering by it is ordering by when it
    /// ran.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn update_jobs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which jobs to list. Omitted means all of them.")] filter: Option<
            UpdateJobFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means id ascending.")]
        order_by: Option<Vec<UpdateJobOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<UpdateJob>> {
        jobs::update_jobs(ctx, filter, order_by, first, after).await
    }

    /// Poll an export of the run store this server started.
    ///
    /// Null when no export carries that id: it was never started, or it has
    /// expired. An export's file is kept for an hour, and its record goes with
    /// the file, so neither outlives the other.
    async fn run_export(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The export job's id.")] id: ID,
    ) -> Option<RunExport> {
        jobs::run_export(ctx, id).await
    }

    /// Every open ask across every run: the approval inbox.
    ///
    /// The daemon holds these in memory, so this is one read rather than a walk
    /// of the run store. Each entry names the run it is parked on through its
    /// own `run` field, which is what a client needs to show the row it
    /// belongs to.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn open_interactions(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which open asks to include. Omitted means all of them.")] filter: Option<
            super::types::interaction::InteractionFilter,
        >,
        #[graphql(desc = "Sort key and direction. Omitted means the order they were read.")]
        order_by: Option<Vec<super::types::interaction::InteractionOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<super::types::interaction::Interaction>> {
        runs::open_interactions(ctx, filter, order_by, first, after).await
    }

    /// Keyset-paged run listing.
    ///
    /// The filter mirrors `RunOutput` itself: a field of the run is a field of
    /// the filter, and `and`, `or` and `not` compose them. Read one run with
    /// `run(id:)`, a batch with `filter: { id: { in: [...] } }`, the runs
    /// nobody started with `filter: { parentId: { isNull: true } }`, and a
    /// whole subtree with `filter: { ancestorIds: { has: "<id>" } }`.
    ///
    /// There is no scan cap. A filter or a search that names a file is answered
    /// by reading, and only for the runs the page being asked for reaches, so
    /// page two costs nothing for page one's runs. `total` is the one field
    /// that can cost a pass over the store: ask for it on the first page.
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which runs to list. Omitted means all of them.")] filter: Option<
            RunFilter,
        >,
        #[graphql(desc = "Free-text search across the listing.")] search: Option<RunSearchOptions>,
        #[graphql(desc = "Sort keys, in priority order. Omitted means newest first.")]
        order_by: Option<Vec<RunOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size limit.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<Cursor>,
    ) -> async_graphql::Result<Connection<Run, RunListingExtras>> {
        runs::runs(ctx, filter, search, order_by, first, after).await
    }

    /// One run, by the id every other route names it by.
    ///
    /// Null for an id nothing answers to. A lookup answers "not here" rather
    /// than failing, so a client reading several ids gets an answer for each.
    async fn run(&self, #[graphql(desc = "The run's id.")] id: async_graphql::ID) -> Option<Run> {
        runs::run(id).await
    }

    /// The daemon's own clock, in unix epoch seconds.
    ///
    /// Every duration a run reports is measured against this. A client drawing
    /// its own clocks should draw them against this rather than the browser's,
    /// which disagrees by whatever the two machines' clocks disagree by.
    async fn server_time(&self) -> Timestamp {
        runs::server_time().await
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
        ctx: &Context<'_>,
        #[graphql(desc = "The manifest text to check.")] manifest: String,
        #[graphql(
            name = "as",
            desc = "Check the text as this installed blueprint, so its own scripts resolve."
        )]
        as_blueprint: Option<BlueprintRef>,
    ) -> async_graphql::Result<ValidationReport> {
        checks::validate_blueprint(ctx, manifest, as_blueprint).await
    }

    /// Whether a provider key looks like one of that provider's.
    ///
    /// Format only: nothing is dialled and nothing is written, which is what
    /// makes it safe to run on every keystroke of a form. `checkProvider` is
    /// the one that asks the account.
    async fn validate_provider_key(
        &self,
        #[graphql(desc = "The provider the key is for.")] provider: String,
        #[graphql(desc = "The key to look at. Never stored, never logged.")] key: String,
        #[graphql(desc = "A gateway's address, checked first when given.")] base_url: Option<
            String,
        >,
    ) -> KeyVerdict {
        checks::validate_provider_key(provider, key, base_url).await
    }

    /// Whether a script compiles, without writing it.
    ///
    /// The alternative was saving it and waiting for a run to fail, which is
    /// not much of an improvement on editing the file over SSH. Ungated:
    /// compiling text in memory writes nothing and runs nothing, because every
    /// compiler here stops at the syntax tree.
    async fn validate_script(
        &self,
        #[graphql(desc = "Which registry the script is for.")] kind: ScriptKind,
        #[graphql(desc = "The source to compile.")] content: String,
        #[graphql(desc = "Hook functions it has to define, for a stage or region hook.")]
        required_hooks: Option<Vec<String>>,
    ) -> async_graphql::Result<ScriptVerdict> {
        checks::validate_script(kind, content, required_hooks).await
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
