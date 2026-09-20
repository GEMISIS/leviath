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
use super::connection::{Highlight, PageInfo, RunConnection, RunEdge};
use super::error::IntoGraphql;
use super::scalars::{Cursor, Timestamp};
use super::types::blueprint::Blueprint;
use super::types::catalog::{Model, Provider, SkippedTool, Tool, ToolGroup, ToolInventory};
use super::types::run::{Run, RunStatus};

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
    fn selection(self, first: i32, ids: Option<Vec<String>>) -> Result<RunSelection, ServeError> {
        let limit = page_size(first)?;
        let parent = match (self.parent.as_deref(), self.top_level_only) {
            (Some(_), Some(true)) => {
                return Err(ServeError::BadRequest(
                    "`parent` names one run's children, so it cannot be combined with \
                     `topLevelOnly`"
                        .to_string(),
                ));
            }
            (Some(id), _) => ParentFilter::Of(id.to_string()),
            (None, Some(true)) => ParentFilter::Roots,
            (None, _) => ParentFilter::Any,
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
        })
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
            desc = "Reload the provider listings before answering.",
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
    /// Scoped to one blueprint's own directory when `agent` names one, which
    /// is what an editor offering an `available_tools` list wants.
    async fn tools(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Scope to this blueprint's own tools directory.")] agent: Option<String>,
    ) -> async_graphql::Result<ToolInventory> {
        let state = ctx.data_unchecked::<AppState>();
        let config = state.current_config();
        let dir = match agent.as_deref() {
            Some(name) => Some(super::super::tools::agent_dir(&config, name).gql()?),
            None => None,
        };
        // The walk over an agent directory belongs on the blocking pool.
        let inventory = blocking(move || {
            crate::tool_inventory::ToolInventory::discover(dir.as_deref(), agent.as_deref())
        })
        .await;
        Ok(ToolInventory {
            tools: inventory
                .tools
                .into_iter()
                .map(|tool| Tool {
                    name: tool.name,
                    source: tool.source.as_str().to_string(),
                    path: tool.path.map(|p| p.display().to_string()),
                    agent: tool.agent,
                })
                .collect(),
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
