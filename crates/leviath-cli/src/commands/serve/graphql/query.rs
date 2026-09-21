//! The read side of the schema.
//!
//! Every field here turns its arguments into a service-layer call and its
//! answer into GraphQL objects. No field reaches for a REST route, and none
//! re-implements a filter: `runs` builds the same [`RunSelection`] the REST
//! listing builds, so the two cannot disagree about which runs match.

use std::sync::Arc;

use async_graphql::{Context, Enum, InputObject, Object};

use super::super::blocking::blocking;
use super::super::core::blueprints;
use super::super::core::error::ServeError;
use super::super::core::runs::{self as run_core, ParentFilter, RunSelection, SortKey, Source};
use super::super::cursor::{self, CursorKey};
use super::super::types::AppState;
use super::checks::{
    KeyVerdict, ScriptVerdict, ValidationReport, YoloDecision, YoloTestInput, field_of,
};
use super::connection::{Highlight, PageInfo, RunConnection, RunEdge};
use super::error::IntoGraphql;
use super::node::Node;
use super::scalars::{BigInt, Cursor, Timestamp};
use super::types::blueprint::Blueprint;
use super::types::catalog::{Model, Provider, SkippedTool, Tool, ToolGroup, ToolInventory};
use super::types::machine::{
    Config, ConfigError, Directory, DoctorCheck, DoctorReport, Gateway, McpServer, MimeRow, Script,
    ServeLimits, YoloHuman, YoloProfile, YoloProfiles, YoloWaiver,
};
use super::types::run::{Run, RunStatus};
use super::types::update::{DaemonStatus, UpdateInfo, UpdateJob};

/// Sort order for the run listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunSort {
    /// Newest spawns first.
    #[graphql(name = "STARTED_AT")]
    Started,
    /// Most recently changed first.
    #[graphql(name = "UPDATED_AT")]
    Updated,
    /// Runs that moved most recently first.
    #[graphql(name = "LAST_PROGRESS_AT")]
    LastProgress,
}

impl From<RunSort> for SortKey {
    fn from(sort: RunSort) -> Self {
        match sort {
            RunSort::Started => SortKey::Started,
            RunSort::Updated => SortKey::Updated,
            RunSort::LastProgress => SortKey::LastProgress,
        }
    }
}

/// Where a run search looks.
///
/// `META` and `FILES` answer from what is already parsed in memory. The other
/// three read files per run, so they are what a client offers as a "search
/// inside runs" toggle rather than paying for on every keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SearchScope {
    /// Run metadata: title, agent name, status, task, caller metadata.
    Meta,
    /// The run's own record of the files it changed.
    Files,
    /// Region contents on disk.
    Context,
    /// Log lines on disk.
    Logs,
    /// The crash-resume journal.
    Journal,
}

impl From<SearchScope> for Source {
    fn from(scope: SearchScope) -> Self {
        match scope {
            SearchScope::Meta => Source::Meta,
            SearchScope::Files => Source::Files,
            SearchScope::Context => Source::Context,
            SearchScope::Logs => Source::Logs,
            SearchScope::Journal => Source::Journal,
        }
    }
}

impl SearchScope {
    /// The word this scope goes on the wire as, for the cursor's filter
    /// digest.
    ///
    /// The digest is taken over the request's own spelling, so a cursor
    /// cannot be carried from one search to a different one that happens to
    /// parse to the same set.
    fn as_str(self) -> &'static str {
        match self {
            SearchScope::Meta => "meta",
            SearchScope::Files => "files",
            SearchScope::Context => "context",
            SearchScope::Logs => "logs",
            SearchScope::Journal => "journal",
        }
    }
}

/// Which runs a listing is about: status, search, parentage and time, in one
/// place.
#[derive(Debug, Default, InputObject)]
pub(crate) struct RunFilter {
    /// Only runs in this status.
    pub(crate) status: Option<RunStatus>,
    /// Runs in any of these statuses. Prefer this over one request per status
    /// when a client wants "everything active" as a single predicate.
    pub(crate) status_in: Option<Vec<RunStatus>>,
    /// Case-insensitive substring. No regex, no boolean operators.
    pub(crate) query: Option<String>,
    /// Where to look. Defaults to metadata and files, which are free.
    pub(crate) query_in: Option<Vec<SearchScope>>,
    /// Direct children of this run id. A run id naming nothing gives an empty
    /// page rather than an error: a run with no children yet is a normal
    /// answer.
    pub(crate) parent: Option<String>,
    /// Only runs nobody started, when true.
    pub(crate) top_level_only: Option<bool>,
    /// Only runs somebody started, at any depth, when true. The mirror of
    /// `topLevelOnly`, and what a "workers only" view asks for.
    pub(crate) sub_agents_only: Option<bool>,
    /// Every run under this one, at any depth, and not the run itself. The flat
    /// read of a fan-out: nesting `children` walks one level per request, and
    /// this walks the whole subtree in one page at a time.
    pub(crate) descendant_of: Option<String>,
    /// Only runs of this blueprint, by the name the run recorded. A name nothing
    /// matches gives an empty page rather than an error.
    pub(crate) blueprint: Option<String>,
    /// Inclusive lower bound on the sort value. Pass the previous page's
    /// `serverTime` to poll for what changed.
    pub(crate) since: Option<Timestamp>,
    /// Newest spawns or most recently active first.
    pub(crate) sort: Option<RunSort>,
    /// Oldest first when true.
    pub(crate) ascending: Option<bool>,
}

impl RunFilter {
    /// Turn the filter into the selection both surfaces list from.
    ///
    /// Rejections happen here, before anything is read: a page size over the
    /// cap, a batch fetch combined with a filter, or more ids than one
    /// request may name.
    pub(crate) fn selection(
        self,
        first: i32,
        ids: Option<Vec<String>>,
    ) -> Result<RunSelection, ServeError> {
        let limit = page_size(first)?;
        // One question about parentage per listing. Two of these together would
        // be two predicates for one field, and the pair a caller meant is not
        // recoverable from the pair they sent.
        let asked: Vec<&str> = [
            self.parent.is_some().then_some("parent"),
            self.descendant_of.is_some().then_some("descendantOf"),
            (self.top_level_only == Some(true)).then_some("topLevelOnly"),
            (self.sub_agents_only == Some(true)).then_some("subAgentsOnly"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if asked.len() > 1 {
            return Err(ServeError::BadRequest(format!(
                "{} each say which runs to include, so only one of them may be set",
                asked.join(" and ")
            )));
        }
        let parent = match (
            self.parent.as_deref(),
            self.descendant_of.as_deref(),
            self.top_level_only,
            self.sub_agents_only,
        ) {
            (Some(id), _, _, _) => ParentFilter::Of(id.to_string()),
            (None, Some(root), _, _) => ParentFilter::Under(root.to_string()),
            (None, None, Some(true), _) => ParentFilter::Roots,
            (None, None, _, Some(true)) => ParentFilter::SubAgents,
            _ => ParentFilter::Any,
        };

        let mut statuses: Vec<String> = Vec::new();
        if let Some(status) = self.status {
            statuses.push(status.wire().to_string());
        }
        for status in self.status_in.into_iter().flatten() {
            statuses.push(status.wire().to_string());
        }

        let scopes = self.query_in.unwrap_or_default();
        let sources_raw = match scopes.is_empty() {
            true => String::new(),
            false => scopes
                .iter()
                .map(|scope| scope.as_str())
                .collect::<Vec<_>>()
                .join(","),
        };
        let sources = match scopes.is_empty() {
            true => vec![Source::Meta, Source::Files],
            false => scopes.into_iter().map(Source::from).collect(),
        };

        if ids.is_some() {
            let conflict = self.query.is_some()
                || !statuses.is_empty()
                || self.since.is_some()
                || parent != ParentFilter::Any;
            if conflict {
                return Err(ServeError::BadRequest(
                    "`ids` names exactly which runs to return, so it cannot be combined with a \
                     filter"
                        .to_string(),
                ));
            }
        }
        if let Some(ref ids) = ids
            && ids.len() > run_core::MAX_IDS
        {
            return Err(ServeError::BadRequest(format!(
                "`ids` names {} runs; at most {} may be fetched at once",
                ids.len(),
                run_core::MAX_IDS
            )));
        }

        Ok(RunSelection {
            limit,
            statuses,
            sort: self.sort.unwrap_or(RunSort::Started).into(),
            descending: !self.ascending.unwrap_or(false),
            q: self.query,
            sources,
            sources_raw,
            fields: None,
            ids,
            since: self.since.map(|t| t.0),
            parent,
            blueprint: self.blueprint,
        })
    }

    /// The same selection, unpaged: every run the filter matches.
    ///
    /// For an export, whose answer is a file rather than a response, so the
    /// page cap has nothing left to protect. It is built through the listing's
    /// own path all the same, so every other bound the listing enforces still
    /// holds.
    pub(crate) fn everything(self) -> Result<RunSelection, ServeError> {
        let mut selection = self.selection(1, None)?;
        selection.limit = usize::MAX;
        Ok(selection)
    }
}

/// Check a requested page size against the cap.
///
/// Refused rather than clamped. REST clamps because a query string is often
/// hand-written and a clamped answer is still useful; a GraphQL client builds
/// its query in code, and silently getting 200 of the 500 it asked for is the
/// kind of bug that only shows up as missing rows much later.
pub(crate) fn page_size(first: i32) -> Result<usize, ServeError> {
    match usize::try_from(first) {
        Ok(0) | Err(_) => Err(ServeError::BadRequest(
            "`first` must be at least 1; omit it for the default".to_string(),
        )),
        Ok(n) if n > run_core::MAX_LIMIT => Err(ServeError::BadRequest(format!(
            "`first` may be at most {}, the server's page-size cap",
            run_core::MAX_LIMIT
        ))),
        Ok(n) => Ok(n),
    }
}

/// The read side: runs, and the fleet's current state.
pub(crate) struct Query;

#[Object]
impl Query {
    /// The blueprints installed on this machine, by name.
    ///
    /// This is the live definition, not what any run executed: for that, read
    /// `blueprint` on the run, which answers from the run's own snapshot. The
    /// digests tell you whether the two are the same bytes.
    ///
    /// `exact` fetches blueprints by name. A name that is not installed lands
    /// in `missing` rather than failing the request.
    async fn blueprints(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Case-insensitive prefix match on the blueprint name.")] query: Option<
            String,
        >,
        #[graphql(desc = "Exact names to fetch; unknown ones land in missing.")] exact: Option<
            Vec<String>,
        >,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "How many to skip, for the page after the first.", default = 0)] skip: i32,
    ) -> async_graphql::Result<BlueprintConnection> {
        let state = ctx.data_unchecked::<AppState>();
        let limit = page_size(first).gql()?;
        let skip = usize::try_from(skip)
            .map_err(|_| ServeError::BadRequest("`skip` cannot be negative".to_string()))
            .gql()?;
        let config = state.current_config();
        // The walk over every agent directory belongs on the blocking pool,
        // and the roots are resolved first so a test's agents-dir override is
        // visible from the task that resolves them.
        let roots = super::super::blueprints::blueprint_roots(&config);
        let installed = blocking(move || super::super::blueprints::discover_in(roots)).await;

        let mut missing = Vec::new();
        let chosen: Vec<_> = match exact {
            Some(names) => {
                let mut kept = Vec::new();
                for name in names {
                    match installed.iter().find(|info| info.name == name) {
                        Some(info) => kept.push(info.clone()),
                        None => missing.push(name),
                    }
                }
                kept
            }
            None => {
                let prefix = query.unwrap_or_default().to_lowercase();
                installed
                    .iter()
                    .filter(|info| info.name.to_lowercase().starts_with(&prefix))
                    .cloned()
                    .collect()
            }
        };

        let total = i32::try_from(chosen.len()).unwrap_or(i32::MAX);
        let page: Vec<_> = chosen.iter().skip(skip).take(limit).collect();
        let has_next_page = chosen.len() > skip.saturating_add(page.len());
        let mut edges = Vec::with_capacity(page.len());
        for info in page {
            // The parse came with the listing row, so there is no second parse
            // here and no failure path: a row exists only because its manifest
            // parsed.
            let manifest = blueprints::ManifestText::installed(info.manifest.clone());
            let parsed = Arc::clone(&info.parsed);
            edges.push(BlueprintEdge {
                node: Blueprint {
                    parsed,
                    digest: manifest.digest,
                    source: manifest.source.into(),
                },
                // The listing is name-sorted and read whole, so the cursor is
                // the name: resuming means "the ones after this name", which
                // survives a blueprint being installed or removed mid-walk.
                cursor: Cursor(info.name.clone()),
            });
        }
        let end_cursor = edges.last().map(|edge| edge.cursor.clone());

        Ok(BlueprintConnection {
            edges,
            page_info: PageInfo {
                has_next_page,
                end_cursor,
            },
            total,
            missing,
        })
    }

    /// How this server is configured, with every secret left out.
    ///
    /// Read `capabilities` before choosing a code path. A 404 also means "no
    /// such run", so discovering a feature by being refused costs a round trip
    /// and tells you less.
    async fn config(&self, ctx: &Context<'_>) -> Config {
        let state = ctx.data_unchecked::<AppState>();
        // One health read rather than a config read beside it: health re-checks
        // the file and hands back the config in force with its verdict, so the
        // two halves of one answer cannot disagree.
        let health = state.config.health();
        config_of(
            &health.config.clone(),
            &state.limits.request_limits,
            &health,
            super::admin::admin_visible(ctx),
        )
    }

    /// Environment and configuration diagnostics.
    ///
    /// A failing check is `ok: false` inside a healthy answer, never an error:
    /// the request to run the checks succeeded, and what they found is the
    /// answer.
    async fn doctor(&self) -> DoctorReport {
        let report = super::super::doctor::offline_report().await;
        doctor_report(report.checks)
    }

    /// The MCP servers this machine has configured.
    async fn mcp_servers(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<McpServer>> {
        let state = ctx.data_unchecked::<AppState>();
        Ok(super::super::mcp::server_infos(state)
            .gql()?
            .into_iter()
            .map(McpServer::from_info)
            .collect())
    }

    /// The yolo profiles, and the file they are read from.
    async fn yolo_profiles(&self) -> YoloProfiles {
        yolo_profiles()
    }

    /// The operator's mime registry, before any blueprint's own rows.
    async fn mime(&self, ctx: &Context<'_>) -> Vec<MimeRow> {
        let state = ctx.data_unchecked::<AppState>();
        super::super::blobs::mime_rows(state)
            .into_iter()
            .map(|row| MimeRow {
                mime_type: row.mime_type,
                source: row.source,
                family: row.family,
                text: row.text,
                extensions: row.extensions.unwrap_or_default(),
            })
            .collect()
    }

    /// The scripts this machine has registered.
    async fn scripts(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only this blueprint's own scripts, plus the global ones.")]
        blueprint: Option<String>,
    ) -> async_graphql::Result<Vec<Script>> {
        let state = ctx.data_unchecked::<AppState>();
        Ok(
            super::super::scripts::registered(state, blueprint.as_deref())
                .gql()?
                .into_iter()
                .map(Script::from_item)
                .collect(),
        )
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
        let state = ctx.data_unchecked::<AppState>();
        let listing = super::super::fs::dir_listing(state, path.as_deref(), hidden).gql()?;
        Ok(Directory {
            path: listing.path,
            parent: listing.parent,
            home: listing.home,
            cwd: listing.cwd,
            entries: listing.dirs.into_iter().map(|dir| dir.name).collect(),
        })
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
        let state = ctx.data_unchecked::<AppState>();
        let query = super::super::config_types::ModelsQuery { provider, refresh };
        let (_, listing) = super::super::config::models_with(state, &query).await;
        listing.0.iter().map(Model::from).collect()
    }

    /// The providers this machine can reach, configured or not.
    ///
    /// `enabled` and `signedIn` are different questions with different
    /// answers: a provider can be turned on with no credential stored, and a
    /// credential can outlive the config entry that used it.
    async fn providers(&self, ctx: &Context<'_>) -> Vec<Provider> {
        let state = ctx.data_unchecked::<AppState>();
        super::super::providers::provider_infos(state)
            .iter()
            .map(Provider::from)
            .collect()
    }

    /// The tools an agent on this machine can call.
    ///
    /// Scoped to one blueprint's own directory when `blueprint` names one,
    /// which is what an editor offering an `available_tools` list wants.
    async fn tools(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Scope to this blueprint's own tools directory.")] blueprint: Option<
            String,
        >,
    ) -> async_graphql::Result<ToolInventory> {
        let state = ctx.data_unchecked::<AppState>();
        let config = state.current_config();
        let dir = match blueprint.as_deref() {
            Some(name) => Some(super::super::tools::agent_dir(&config, name).gql()?),
            None => None,
        };
        // The walk over a blueprint's own directory belongs on the blocking
        // pool.
        let inventory = blocking(move || {
            crate::tool_inventory::ToolInventory::discover(dir.as_deref(), blueprint.as_deref())
        })
        .await;
        Ok(ToolInventory {
            tools: inventory.tools.into_iter().map(Tool::of).collect(),
            groups: leviath_core::blueprint::ToolGroup::ALL
                .iter()
                .map(|group| ToolGroup {
                    name: group.token().to_string(),
                    description: group.describe().to_string(),
                })
                .collect(),
            skipped: inventory
                .skipped
                .into_iter()
                .map(|skipped| SkippedTool {
                    path: skipped.path.display().to_string(),
                    reason: skipped.reason,
                })
                .collect(),
        })
    }

    /// Who is on the other end of the control socket.
    ///
    /// A read this server answers from what it already knows, so it works while
    /// the daemon is down: that is the point of asking. `connected` false does
    /// not mean requests fail, it means the live frames have stopped.
    async fn daemon(&self, ctx: &Context<'_>) -> DaemonStatus {
        let state = ctx.data_unchecked::<AppState>();
        DaemonStatus::of(state.control.link(), state.control.code_mismatch())
    }

    /// What an update would do, and whether there is anything newer to get.
    ///
    /// Planning never reaches the network. The "is there anything newer" half is
    /// whatever the last check found, and asking starts another one for whoever
    /// asks next rather than waiting on one here, so this is cheap enough for a
    /// page to ask every time it opens.
    async fn update(&self, ctx: &Context<'_>) -> UpdateInfo {
        let state = ctx.data_unchecked::<AppState>();
        let plan = super::super::update::planned();
        if state.current_config().update_check {
            state.update_check.read_and_maybe_refresh(
                plan.method.channel(),
                super::super::config_types::API_VERSION,
            );
        }
        UpdateInfo::from_plan(
            &plan,
            super::super::config_types::API_VERSION,
            &state.update_check.peek(),
        )
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
        super::node::resolve(ctx, id.as_str()).await
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
        let state = ctx.data_unchecked::<AppState>();
        state.update_jobs.get(&id).map(UpdateJob::from)
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
        let state = ctx.data_unchecked::<AppState>();
        state
            .caches
            .exports
            .get(&id)
            .map(|job| BulkExport::from_job(state, &job))
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
        let state = ctx.data_unchecked::<AppState>();
        let open = super::super::core::spawn::open_interactions(state)
            .await
            .gql()?;
        Ok(open
            .into_iter()
            .map(|(run_id, request)| OpenInteraction {
                run_id,
                request: request.into(),
            })
            .collect())
    }

    /// Keyset-paged run listing.
    ///
    /// `ids` fetches exact runs, which is also how a client reads one run:
    /// `runs(ids: ["..."])`. An id that names nothing lands in `missing`
    /// rather than failing the request, so one dead id in a batch of fifty
    /// does not cost the other forty-nine.
    async fn runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Status, search, parentage and time filters.")] filter: Option<RunFilter>,
        #[graphql(
            desc = "Page size; capped by the server's page-size limit.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page's pageInfo.")] after: Option<Cursor>,
        #[graphql(desc = "Exact ids to fetch; unknown ones land in missing.")] ids: Option<
            Vec<String>,
        >,
    ) -> async_graphql::Result<RunConnection> {
        let state = ctx.data_unchecked::<AppState>();
        let selection = filter.unwrap_or_default().selection(first, ids).gql()?;
        let sort = selection.sort;
        let descending = selection.descending;
        let spec = selection
            .resolve(after.as_ref().map(|c| c.0.as_str()))
            .gql()?;
        let listing = run_core::list(state, &spec).await;

        let has_next_page = listing.next_cursor.is_some();
        let end_cursor = listing.next_cursor.map(Cursor);
        let now = listing.server_time;
        let edges = listing
            .hits
            .into_iter()
            .map(|hit| {
                let cursor = Cursor(cursor::encode(
                    sort.as_str(),
                    if descending { "desc" } else { "asc" },
                    &spec.digest,
                    CursorKey::Int(sort.value(&hit.meta)),
                    &hit.meta.run_id,
                ));
                RunEdge {
                    node: Run {
                        meta: hit.meta,
                        now,
                    },
                    cursor,
                    highlights: hit
                        .highlights
                        .into_iter()
                        .map(|h| Highlight {
                            field: h.field,
                            snippet: h.snippet,
                            stage: h.stage.and_then(|s| i32::try_from(s).ok()),
                        })
                        .collect(),
                }
            })
            .collect();

        Ok(RunConnection {
            edges,
            page_info: PageInfo {
                has_next_page,
                end_cursor,
            },
            total: listing.total.and_then(|t| i32::try_from(t).ok()),
            scan_truncated: listing.scan_truncated,
            missing: listing.missing,
            server_time: Timestamp(now),
        })
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
        #[graphql(desc = "The manifest text to check.")] manifest: String,
        #[graphql(desc = "Check it as this installed blueprint, so its own scripts resolve.")]
        blueprint: Option<String>,
    ) -> async_graphql::Result<ValidationReport> {
        let dir = match blueprint.as_deref() {
            Some(name) => blueprints::blueprint_dir(name).gql()?,
            None => std::path::PathBuf::new(),
        };
        let report = super::super::blueprints::validate_manifest_text(&manifest, &dir);
        Ok(ValidationReport {
            valid: report.valid,
            errors: report.errors.unwrap_or_default(),
            warnings: report.warnings.unwrap_or_default(),
        })
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
        let checked = super::super::config::checked_key(&provider, &key, base_url.as_deref());
        KeyVerdict {
            valid: checked.valid,
            message: checked.message,
        }
    }

    /// Whether a script compiles, without writing it.
    ///
    /// The alternative was saving it and waiting for an agent to fail, which is
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
        let hooks = hooks.unwrap_or_default();
        let named: Vec<&str> = hooks.iter().map(String::as_str).collect();
        let checked = super::super::scripts::compiled(&kind, &content, &named).gql()?;
        Ok(ScriptVerdict {
            valid: checked.valid,
            error: checked.error,
        })
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
        let state = ctx.data_unchecked::<AppState>();
        let decided = super::super::yolo::decided(
            state,
            super::super::yolo::TestReq {
                profile: call.profile,
                tool: call.tool,
                command: call.command,
                arguments: call.arguments.map(|json| json.0),
                workdir: call.workdir,
                configured: call.configured,
                kind: call.kind,
                allowed: call.allowed.unwrap_or(false),
            },
        )
        .gql()?;
        Ok(YoloDecision {
            profile: field_of(&decided, "profile"),
            tool: field_of(&decided, "tool"),
            configured: field_of(&decided, "configured"),
            policy: field_of(&decided, "policy"),
            reason: decided
                .get("reason")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        })
    }
}

/// What a profile's default does, in the word the config file uses.
fn waiver_word(waiver: crate::yolo::rules::Waiver) -> YoloWaiver {
    match waiver {
        crate::yolo::rules::Waiver::Allow => YoloWaiver::Allow,
        crate::yolo::rules::Waiver::Ask => YoloWaiver::Ask,
    }
}

/// Whether a human-in-the-loop mechanism reaches a person, in the same words.
fn human_word(human: crate::yolo::rules::Human) -> YoloHuman {
    match human {
        crate::yolo::rules::Human::Ask => YoloHuman::Ask,
        crate::yolo::rules::Human::Auto => YoloHuman::Auto,
    }
}

/// The yolo profiles as this schema describes them.
///
/// Shared by the field and the write, so "what is there now" is one shape
/// whichever asked.
pub(crate) fn yolo_profiles() -> YoloProfiles {
    let listing = super::super::yolo::listing();
    YoloProfiles {
        path: listing.path,
        exists: listing.exists,
        error: listing.error,
        profiles: listing
            .profiles
            .into_iter()
            .map(|profile| YoloProfile {
                id: super::node::yolo_profile_id(&profile.name),
                name: profile.name,
                default: waiver_word(profile.default),
                questions: human_word(profile.questions),
                checkpoints: human_word(profile.checkpoints),
                gate: human_word(profile.gate),
                tool_rules: profile.tool_rules.iter().map(|n| count(*n)).collect(),
                shell_rules: profile.shell_rules.iter().map(|n| count(*n)).collect(),
            })
            .collect(),
    }
}

/// The config as this schema describes it, with every secret left out.
///
/// Shared with the write side, so a config read and the answer to a config write
/// are the same shape rather than two that drifted.
pub(crate) fn config_of(
    config: &crate::config::Config,
    requests: &super::super::request_limits::RequestLimits,
    health: &crate::daemon::config_reload::ConfigHealth,
    admin_enabled: bool,
) -> Config {
    let redacted = super::super::config::redact(config, requests, health);
    let mut configured = Vec::new();
    for (name, present) in [
        ("anthropic", redacted.has_anthropic_key),
        ("openai", redacted.has_openai_key),
        ("google", redacted.has_google_key),
        ("openrouter", redacted.has_openrouter_key),
        ("bedrock", redacted.has_bedrock_key),
        ("xai", redacted.has_xai_key),
        ("meta", redacted.has_meta_key),
    ] {
        if present {
            configured.push(name.to_string());
        }
    }
    Config {
        default_provider: redacted.default_provider,
        provider_order: redacted.provider_order,
        override_model: redacted.override_model,
        fallback_model: redacted.fallback_model,
        configured_providers: configured,
        gateways: redacted
            .gateways
            .iter()
            .map(|gateway| Gateway {
                name: gateway.name.clone(),
                base_url: gateway.base_url.clone(),
                has_api_key: gateway.has_api_key,
                kind: gateway.kind.clone(),
            })
            .collect(),
        agent_paths: redacted
            .agent_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        mcp_server_count: count(redacted.mcp_server_count),
        api_version: redacted.api_version,
        capabilities: redacted.capabilities,
        admin_enabled,
        limits: ServeLimits {
            max_page_size: count(redacted.limits.max_limit),
            max_ids: count(redacted.limits.max_ids),
            max_file_bytes: BigInt(redacted.limits.max_file_bytes as i64),
            max_listing_entries: count(redacted.limits.max_listing_entries),
            max_search_scan: count(redacted.limits.max_search_scan),
            max_history_limit: count(redacted.limits.max_history_limit),
            max_concurrent_requests: BigInt(redacted.limits.max_concurrent_requests as i64),
            max_upload_bytes: BigInt(requests.max_upload_bytes as i64),
            request_timeout_secs: i32::try_from(requests.request_timeout_secs).unwrap_or(i32::MAX),
        },
        config_error: redacted.config_error.map(|error| ConfigError {
            kind: error.kind,
            path: error.path,
            message: error.message,
            line: error.line.and_then(|line| i32::try_from(line).ok()),
            column: error.column.and_then(|col| i32::try_from(col).ok()),
            key: error.key,
            since: Timestamp(error.since),
            note: error.note,
        }),
        config_mtime: redacted.config_mtime.map(Timestamp),
    }
}

/// One diagnostics run as this schema describes it.
///
/// Shared by the offline field and the live mutation: they run different checks
/// and answer with the same shape, which is what lets a client render one view.
pub(crate) fn doctor_report(checks: Vec<super::super::types::DoctorCheck>) -> DoctorReport {
    DoctorReport {
        ok: checks.iter().all(|check| check.ok),
        checks: checks
            .into_iter()
            .map(|check| DoctorCheck {
                name: check.name,
                ok: check.ok,
                detail: check.detail,
            })
            .collect(),
    }
}

/// Narrow a count to the 32 bits GraphQL's `Int` carries.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// An export job, as a client polls it.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct BulkExport {
    /// The job's id, which `bulkExport` and `node` both take. Unique to this
    /// server: the jobs live in memory, so nothing answers to it after a
    /// restart.
    #[graphql(owned)]
    pub(crate) id: async_graphql::ID,
    /// Where it has got to: `queued`, `running`, `complete` or `failed`.
    pub(crate) status: String,
    /// How many runs have been written.
    pub(crate) written: i32,
    /// Why it failed, when it did.
    pub(crate) error: Option<String>,
    /// A short-lived signed link to the JSONL. Null until the export is
    /// complete, because there is nothing to fetch before then.
    pub(crate) download_url: Option<String>,
}

impl BulkExport {
    /// Describe a job, minting its link once there is a file to fetch.
    pub(crate) fn from_job(state: &AppState, job: &super::super::core::export::ExportJob) -> Self {
        let complete = job.status == super::super::core::export::ExportStatus::Complete;
        Self {
            id: async_graphql::ID(job.id.clone()),
            status: job.status.wire().to_string(),
            written: count(job.written),
            error: job.error.clone(),
            download_url: complete.then(|| {
                super::super::signed_url::signed_path(
                    &state.signer,
                    &format!("/api/exports/{}", job.id),
                    &[],
                    leviath_core::duration::now_secs(),
                )
            }),
        }
    }
}

/// One open ask, with the run it is parked on.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct OpenInteraction {
    /// The run waiting for this answer.
    pub(crate) run_id: String,
    /// What is being asked.
    pub(crate) request: super::events::InteractionRequest,
}

/// One installed blueprint with its page cursor.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct BlueprintEdge {
    /// The blueprint.
    pub(crate) node: Blueprint,
    /// Cursor for this edge.
    pub(crate) cursor: Cursor,
}

/// A paged listing of the installed blueprints.
#[derive(async_graphql::SimpleObject)]
pub(crate) struct BlueprintConnection {
    /// The blueprints on this page.
    pub(crate) edges: Vec<BlueprintEdge>,
    /// Keyset page state.
    pub(crate) page_info: PageInfo,
    /// How many blueprints are installed.
    pub(crate) total: i32,
    /// Names from an `exact` fetch that are not installed. Never fails the
    /// request.
    pub(crate) missing: Vec<String>,
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
