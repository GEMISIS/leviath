//! The `runs`, `run`, `serverTime`, `openInteractions` and `node` fields: the
//! run listing, one run by id, the daemon's clock, the approval inbox, and the
//! one lookup that answers from an id alone.
//!
//! The listing is the shape every listing in this schema takes: a filter that
//! mirrors the type it selects, an order built from the type's own sort keys,
//! and a keyset cursor bound to both. What is particular to runs is that the
//! filter may name a file - a run's stages, its context window, its answer -
//! and that the walk in `core::runs` is written so that reading only starts
//! once it reaches the page being asked for.

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, InputObject, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::super::blocking::blocking;
use super::super::super::core::runs::{self as run_core, SortKey, Source};
use super::super::super::types::AppState;
use super::super::connection::{Connection, Total};
use super::super::error::IntoGraphql;
use super::super::filter::run_predicate;
use super::super::paging::order::{OrderDirection, Term};
use super::super::paging::page::page;
use super::super::scalars::{Cursor, Timestamp};
use super::super::types::run::{Run, RunFilter, RunOrder, RunOrderField};

/// What the cap on a run page is called when a request goes over it.
const CAP_NAME: &str = "the run page cap";

/// Where a search looks when it does not say.
///
/// The two sources answerable from the parsed record, and the same default
/// `GET /api/runs` takes - which is what keeps an unfiltered cursor from either
/// surface readable by the other.
const DEFAULT_SOURCES: &str = "meta,files";

/// Where a run search looks.
///
/// `META` and `FILES` answer from what is already parsed in memory. The other
/// three read files per run, so they are what a client offers as a "search
/// inside runs" toggle rather than paying for on every keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SearchScope {
    /// Run metadata: title, blueprint name, status, task, caller metadata.
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
    /// The word this scope goes on the wire as, for the cursor's digest.
    ///
    /// The digest is taken over the request's own spelling, so a cursor cannot
    /// be carried from one search to a different one that happens to resolve to
    /// the same set. It is also the word `?q_in=` takes.
    fn wire(self) -> &'static str {
        match self {
            SearchScope::Meta => "meta",
            SearchScope::Files => "files",
            SearchScope::Context => "context",
            SearchScope::Logs => "logs",
            SearchScope::Journal => "journal",
        }
    }
}

/// Free-text search across a run listing.
///
/// Separate from the filter because it describes the listing rather than a run:
/// there is one search per request, and nesting one inside `and` would be
/// asking for a second. Case-insensitive substring; no regex and no boolean
/// operators, which the filter's own combinators already say better.
#[derive(Debug, InputObject)]
pub(crate) struct RunSearchOptions {
    /// The text to look for. Case-insensitive, matched as a substring.
    pub(crate) query: String,
    /// Where to look. Omitted means the two sources that cost no read.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<SearchScope>>,
}

/// Where a search matched one run, and enough text to show a person why.
///
/// The part of search a browser cannot do: the client never holds a run's
/// transcript, so without this a deep match is an unexplained result. Keyed by
/// run rather than carried on the run, so a `RunOutput` is the same object
/// wherever it is read.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunHighlight {
    /// The run this match is in.
    pub(crate) run_id: ID,
    /// What matched: a run field name, `metadata.<key>`, `modified_files`,
    /// `context.<region>`, `logs.output`, `logs.operational`, or
    /// `journal.tool.<tool_name>`.
    pub(crate) field: String,
    /// The matching text, with a little either side.
    pub(crate) snippet: String,
    /// Which stage the match came from, for the sources that have one.
    pub(crate) stage_index: Option<i32>,
}

/// What a run listing says beyond the three fields every listing has.
#[derive(Debug, Default, SimpleObject)]
pub(crate) struct RunListingExtras {
    /// Why each run on this page matched the search. Empty when there was no
    /// search.
    pub(crate) highlights: Vec<RunHighlight>,
}

/// The sort key one of the mirror's order fields names.
///
/// The three timestamps are the three `GET /api/runs` takes, spelled the same
/// way, so a cursor minted under one surface names the same walk under the
/// other. The title is this listing's own.
fn sort_key(field: RunOrderField) -> SortKey {
    match field {
        RunOrderField::StartedAt => SortKey::Started,
        RunOrderField::UpdatedAt => SortKey::Updated,
        RunOrderField::LastProgressAt => SortKey::LastProgress,
        RunOrderField::Title => SortKey::Title,
    }
}

/// The order a listing runs in: what was asked for, or newest spawns first.
fn terms_of(order_by: Option<Vec<RunOrder>>) -> Vec<Term<SortKey>> {
    let asked: Vec<RunOrder> = order_by.unwrap_or_default();
    match asked.is_empty() {
        true => vec![Term {
            field: SortKey::Started,
            direction: OrderDirection::Desc,
        }],
        false => asked
            .into_iter()
            .map(|order| {
                let term = order.term();
                Term {
                    field: sort_key(term.field),
                    direction: term.direction,
                }
            })
            .collect(),
    }
}

/// The search half of a listing: the text, the sources, and their spelling.
fn search_of(search: Option<RunSearchOptions>) -> (Option<String>, Vec<Source>, String) {
    let Some(search) = search else {
        // No search, and the default sources all the same, so an unfiltered
        // listing digests exactly as the unfiltered REST one does.
        return (None, default_sources(), DEFAULT_SOURCES.to_string());
    };
    let scopes: Vec<SearchScope> = search.within.unwrap_or_default();
    match scopes.is_empty() {
        true => (
            Some(search.query),
            default_sources(),
            DEFAULT_SOURCES.to_string(),
        ),
        false => {
            let raw = scopes
                .iter()
                .map(|scope| scope.wire())
                .collect::<Vec<_>>()
                .join(",");
            (
                Some(search.query),
                scopes.into_iter().map(Source::from).collect(),
                raw,
            )
        }
    }
}

/// The two sources a search looks in when it names none.
fn default_sources() -> Vec<Source> {
    vec![Source::Meta, Source::Files]
}

/// One page of the run listing, from a filter that has already been composed.
///
/// The one path `runs` and `Run.children` both take, so a child listing is the
/// run listing with the parent preset and not a second walk with its own
/// rules.
async fn listing(
    ctx: &Context<'_>,
    filter: RunFilter,
    search: Option<RunSearchOptions>,
    order_by: Option<Vec<RunOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Run, RunListingExtras>> {
    let state = ctx.data_unchecked::<AppState>();
    let limit = page(first, run_core::MAX_LIMIT, CAP_NAME).gql()?;
    let terms = terms_of(order_by);
    let (query, sources, sources_raw) = search_of(search);

    let mut asked = run_predicate::asking(filter, limit).gql()?;
    // The primary term is what the record filters read and what breaks a tie,
    // and the whole list is what the walk compares on.
    asked.sort = terms[0].field;
    asked.descending = terms[0].direction.descending();
    asked.order = Some(terms);
    asked.q = query;
    asked.sources = sources;
    asked.sources_raw = sources_raw;

    let spec = asked
        .resolve(after.as_ref().map(|token| token.0.as_str()))
        .gql()?;
    let mut walked = run_core::walk(state, &spec).await;
    let results: Vec<Arc<crate::runstate::RunMeta>> = std::mem::take(&mut walked.items);
    let cursor = run_core::lazy::next_cursor(&walked, &spec.digest).map(Cursor);

    let highlights = match spec.q.as_deref() {
        None => Vec::new(),
        Some(query) => highlights(&results, query, &spec.sources),
    };
    // One instant for the whole answer, so two runs on one page do not report
    // ages taken a moment apart.
    let now = leviath_core::duration::now_secs();
    let runs: Vec<Run> = results.into_iter().map(|meta| Run { meta, now }).collect();
    // Counting settles every run the page did not reach, so it runs only where
    // the client selected `total`.
    Ok(Connection::new(
        runs,
        cursor,
        Total::lazy(async move { walked.total().await }),
        RunListingExtras { highlights },
    ))
}

/// Why each run on the page matched, for the search that was run.
fn highlights(
    runs: &[Arc<crate::runstate::RunMeta>],
    query: &str,
    sources: &[Source],
) -> Vec<RunHighlight> {
    runs.iter()
        .flat_map(|meta| {
            run_core::matching::highlights_for(meta, query, sources)
                .into_iter()
                .map(|hit| RunHighlight {
                    run_id: ID(meta.run_id.clone()),
                    field: hit.field,
                    snippet: hit.snippet,
                    stage_index: hit.stage.and_then(|at| i32::try_from(at).ok()),
                })
        })
        .collect()
}

/// Keyset-paged run listing.
///
/// Every run on this machine, filtered by a mirror of `RunOutput` itself: a
/// field of the run is a field of the filter, wrapped in the comparisons its
/// own type takes, and `and`, `or` and `not` compose them. Read one run with
/// `run(id:)` and a batch with `filter: { id: { in: [...] } }`, which reads
/// those records directly rather than walking the store.
///
/// There is no scan cap. A filter that names a file - `stages`, `context`,
/// `finalOutput` - is answered by reading, and the walk only reads for the runs
/// the page it was asked for actually reaches. So page two costs nothing for
/// page one's runs, and `total` is the one field that can cost a pass over the
/// store: ask for it on the first page rather than on every one.
pub(crate) async fn runs(
    ctx: &Context<'_>,
    filter: Option<RunFilter>,
    search: Option<RunSearchOptions>,
    order_by: Option<Vec<RunOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Run, RunListingExtras>> {
    listing(
        ctx,
        filter.unwrap_or_default(),
        search,
        order_by,
        first,
        after,
    )
    .await
}

/// One run's direct children, as the run listing with the parent preset.
///
/// The preset is part of the filter rather than beside it, so it is part of the
/// cursor's digest too: a cursor from one run's children cannot resume another
/// run's.
pub(crate) async fn children_of(
    ctx: &Context<'_>,
    parent_id: &str,
    filter: Option<RunFilter>,
    order_by: Option<Vec<RunOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Run, RunListingExtras>> {
    let preset = RunFilter {
        parent_id: Some(Box::new(super::super::filter::scalars::IDFilter {
            eq: Some(ID(parent_id.to_string())),
            ..Default::default()
        })),
        and: filter.map(|each| vec![each]),
        ..RunFilter::default()
    };
    listing(ctx, preset, None, order_by, first, after).await
}

/// One run, by the id every other route names it by.
///
/// Null for an id nothing answers to: a deleted run, an expired one and a typo
/// are the same answer, and all three mean the run is not here. Read straight
/// from the run's own record rather than through the index, so it costs one
/// file whatever the store holds.
pub(crate) async fn run(id: ID) -> Option<Run> {
    let now = leviath_core::duration::now_secs();
    let wanted = id.as_str().to_string();
    let meta = blocking(move || crate::runstate::read_meta(&wanted).ok()).await?;
    Some(Run {
        meta: Arc::new(meta),
        now,
    })
}

/// The daemon's own clock, in unix epoch seconds.
///
/// Every duration a run reports is measured against this, so a client that
/// draws its own clocks should draw them against this rather than against the
/// browser's: the two disagree by whatever the machine's clocks disagree by.
pub(crate) async fn server_time() -> Timestamp {
    Timestamp(leviath_core::duration::now_secs())
}

/// Every open ask across every run: the approval inbox.
///
/// The daemon holds these in memory, so this is one read rather than a walk
/// of the run store. Each entry names the run it is parked on through its own
/// `run` field, which is what a client needs to show the row it belongs to.
///
/// Keyset-paged like every listing in this schema, though the daemon holds so
/// few of these open at once that a client is unlikely to see a second page.
pub(crate) async fn open_interactions(
    ctx: &Context<'_>,
    filter: Option<super::super::types::interaction::InteractionFilter>,
    order_by: Option<Vec<super::super::types::interaction::InteractionOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<super::super::types::interaction::Interaction>> {
    let state = ctx.data_unchecked::<AppState>();
    let open = super::super::super::core::spawn::open_interactions(state)
        .await
        .gql()?;
    super::super::types::interaction::open(open, filter, order_by, first, after).await
}
