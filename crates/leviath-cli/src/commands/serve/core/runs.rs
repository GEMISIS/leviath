//! The run listing itself: which runs a request is about, in what order, and
//! which page of them.
//!
//! Both surfaces ask the same question here. `GET /api/runs` turns query
//! parameters into a [`RunSpec`] and renders the answer as JSON; the GraphQL
//! `runs` field builds the same spec from its arguments and resolves the
//! answer field by field. Neither one re-implements a filter, a sort or a
//! cursor, which is the point: a run that a REST filter keeps and a GraphQL
//! filter drops would be a bug nobody could explain.
//!
//! Every listing starts from the shared run index (`run_index`), which parses
//! a `meta.json` only when its stat changes, so a page of fifty costs a stat
//! per live run rather than a parse of every run on the machine.
//! [`MAX_SEARCH_SCAN`] bounds the half of search that reads files.

use std::collections::HashSet;
use std::sync::Arc;

use super::super::cursor::{self, Cursor, CursorKey};
use super::super::graphql::paging::order::{Order, OrderDirection, Term};
use super::super::search;
use super::super::types::{AppState, Highlight, status_matches};
use crate::runstate::{self, RunMeta};

pub(crate) mod lazy;
pub(crate) mod matching;
pub(crate) mod predicate;
use matching::*;
use predicate::{MatchContext, RunPredicate, RunTree};

/// Largest page size served. A larger `limit` is clamped rather than refused: a
/// client asking for 1000 wants as much as it can get, and the real value is
/// discoverable from `GET /api/config`.
pub(crate) const MAX_LIMIT: usize = 200;
/// Most ids one batch fetch may name.
pub(crate) const MAX_IDS: usize = 200;
/// How many runs a filesystem-reading search will examine before giving up.
///
/// `q_in=logs` over an unbounded, never-pruned run set is a self-inflicted
/// denial of service: every request would read two files per stage per run, for
/// every run that has ever existed. Stopping after a bounded prefix - taken in
/// the requested sort order, so it is the newest runs - answers the common case
/// and says so via `scan_truncated`, which is better than refusing the query or
/// than quietly taking longer every month.
pub(crate) const MAX_SEARCH_SCAN: usize = 500;
/// How much of each stage log a search reads, from the end.
pub(crate) const SEARCH_LOG_TAIL_BYTES: u64 = 256 * 1024;
/// Most highlights attached to one item. A log with ten thousand matches must
/// not become the response body.
pub(crate) const MAX_HIGHLIGHTS: usize = 5;

/// Which field a run is ordered by.
///
/// The three timestamps `?sort=` takes, each named for the `RunMeta` field it
/// reads, plus the title, which only the GraphQL listing offers: `parse` below
/// still answers for the three alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortKey {
    Started,
    Updated,
    LastProgress,
    Title,
}

impl SortKey {
    /// The key a `?sort=` value names, or nothing for a word this route does
    /// not order by.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "started_at" => Some(SortKey::Started),
            "updated_at" => Some(SortKey::Updated),
            "last_progress_at" => Some(SortKey::LastProgress),
            _ => None,
        }
    }

    /// The word a cursor records for this key, which is also the query value
    /// that selects it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SortKey::Started => "started_at",
            SortKey::Updated => "updated_at",
            SortKey::LastProgress => "last_progress_at",
            SortKey::Title => "title",
        }
    }

    /// This run's place in the order this key runs in.
    ///
    /// `last_progress_at` is `Option`, and absent means "written by a daemon
    /// older than the field, or before the first snapshot landed". The run
    /// demonstrably started, so `started_at` is the honest floor - and it keeps
    /// the key non-null, which the cursor needs. A run with no title yet has no
    /// value at all, and sorts where the cursor's own ordering puts an absent
    /// one.
    pub(crate) fn key(self, meta: &RunMeta) -> CursorKey {
        match self {
            SortKey::Started => CursorKey::Int(meta.started_at),
            SortKey::Updated => CursorKey::Int(meta.updated_at),
            SortKey::LastProgress => {
                CursorKey::Int(meta.last_progress_at.unwrap_or(meta.started_at))
            }
            SortKey::Title => match meta.title {
                Some(ref title) => CursorKey::Text(title.clone()),
                None => CursorKey::Null,
            },
        }
    }
}

/// Where search looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Source {
    /// Fields already parsed into `RunMeta`. No IO.
    Meta,
    /// The tracked modified-file paths. No IO.
    Files,
    /// The run's current context window, as raw unparsed bytes.
    Context,
    /// The tail of each stage's logs, as raw bytes.
    Logs,
    /// The run journal, as raw bytes.
    Journal,
}

impl Source {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "meta" => Some(Source::Meta),
            "files" => Some(Source::Files),
            "context" => Some(Source::Context),
            "logs" => Some(Source::Logs),
            "journal" => Some(Source::Journal),
            _ => None,
        }
    }

    /// Does answering this source require reading files?
    ///
    /// Only these count against [`MAX_SEARCH_SCAN`] - the in-memory sources are
    /// free and must not consume the budget.
    pub(crate) fn reads_filesystem(self) -> bool {
        matches!(self, Source::Context | Source::Logs | Source::Journal)
    }
}

/// Which runs a listing is about.
///
/// A run's sub-agents are runs, so a console that draws them nested under the
/// run that started them was paging by a unit it does not display: a page of
/// fifty could be seven visible rows and forty-three workers hanging off them,
/// and there was no way to ask for anything better. This is that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParentFilter {
    /// No `parent` given: every run, sub-agents included. What this route has
    /// always returned, so an existing caller sees nothing change.
    Any,
    /// `parent=none`: only runs nobody started. What a top-level list wants,
    /// and what makes `total` a count of the rows a client will actually draw.
    Roots,
    /// `parent=<run_id>`: that run's direct children. `GET
    /// /api/agents/{id}/children` answers the same question in one unpaged,
    /// unsorted array, which a fan-out of two hundred workers has no windowed
    /// form of.
    Of(String),
    /// `parent=sub`: every run somebody started, at any depth. The mirror of
    /// [`Roots`](Self::Roots), and what a "workers only" view asks for.
    SubAgents,
    /// `descendant_of=<run_id>`: that run's whole subtree, at any depth, and not
    /// the run itself. A flat read of a fan-out, which nesting `children` can
    /// only do one level per request.
    Under(String),
}

impl ParentFilter {
    /// `none` is the only keyword. Nothing else can collide with it: a run id
    /// is `<agent>-<timestamp>-<hash>`, so no run is ever called `none`.
    ///
    /// An empty value reads as absent rather than as a filter matching nothing,
    /// which is what a client that built its query string from an empty box
    /// meant. Anything else is taken as a run id, and a run id that names
    /// nothing gives an empty page - the same answer `status=` gives for a
    /// status nothing is in, rather than a 404 for a run that may simply have
    /// no children yet.
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => Self::Any,
            Some("none") => Self::Roots,
            Some("sub") => Self::SubAgents,
            Some(id) => Self::Of(id.to_string()),
        }
    }

    /// Whether this run belongs in the listing.
    pub(crate) fn keeps(&self, meta: &RunMeta) -> bool {
        match self {
            Self::Any => true,
            Self::Roots => meta.parent_run_id.is_none(),
            Self::SubAgents => meta.parent_run_id.is_some(),
            Self::Of(parent) => meta.parent_run_id.as_deref() == Some(parent.as_str()),
            // A subtree cannot be decided from one record: it needs the tree.
            // [`keeps_in`] is the form that has it, and the listing uses that.
            Self::Under(_) => true,
        }
    }

    /// Whether this run belongs in the listing, given the tree it is part of.
    ///
    /// The same question as [`keeps`](Self::keeps) for every filter but a
    /// subtree, which is the one that cannot be answered from a single record:
    /// a grandchild names its parent and not its ancestor.
    pub(crate) fn keeps_in(&self, meta: &RunMeta, descendants: &HashSet<String>) -> bool {
        match self {
            Self::Under(_) => descendants.contains(&meta.run_id),
            _ => self.keeps(meta),
        }
    }

    /// This filter's contribution to the cursor digest, so a walk cannot change
    /// what it is filtering halfway through.
    ///
    /// `None` for [`Any`](Self::Any), which contributes nothing at all rather
    /// than an empty part - an empty part is still a part, and would have
    /// changed the digest of every unfiltered listing and so invalidated every
    /// cursor a client was holding when it upgraded.
    pub(crate) fn digest_part(&self) -> Option<String> {
        match self {
            Self::Any => None,
            Self::Roots => Some("none".to_string()),
            Self::SubAgents => Some("sub".to_string()),
            Self::Of(parent) => Some(parent.clone()),
            // Prefixed, so "this run's children" and "this run's whole subtree"
            // are two filters to a cursor rather than one: the two answer
            // different sets, and a cursor from either would otherwise resume in
            // the other.
            Self::Under(root) => Some(format!("under:{root}")),
        }
    }
}

/// A validated run listing request: what to keep, how to order it, and
/// which page.
///
/// Every rejection a listing can produce is decided while one of these is
/// built, so [`list`] below is a straight-line composition with no error path
/// of its own, and each rejection is reachable from a plain unit test.
///
/// `fields` is carried here rather than applied here: projection is how one
/// surface renders a run, and GraphQL does it with a selection set instead.
pub(crate) struct RunSpec {
    pub(crate) limit: usize,
    pub(crate) cursor: Option<Cursor>,
    pub(crate) statuses: Vec<String>,
    pub(crate) sort: SortKey,
    pub(crate) descending: bool,
    /// The compiled order, which is what the lazy walk compares on and what
    /// mints its cursors.
    ///
    /// One term for a listing ordered by one key, which is every listing `GET
    /// /api/runs` serves and what `sort` and `descending` above spell; the
    /// GraphQL field may ask for several, and then this is the whole of them.
    pub(crate) order: Order<SortKey>,
    pub(crate) q: Option<String>,
    pub(crate) sources: Vec<Source>,
    pub(crate) fields: Option<HashSet<String>>,
    pub(crate) ids: Option<Vec<String>>,
    pub(crate) since: Option<i64>,
    pub(crate) parent: ParentFilter,
    /// Only runs of this blueprint, by recorded name.
    pub(crate) blueprint: Option<String>,
    /// A composable predicate the surface built, consulted per run beside the
    /// filters above.
    pub(crate) predicate: Option<Arc<dyn RunPredicate>>,
    /// The records behind `ids`, where the caller already holds them.
    ///
    /// A caller that walked the store to work out which runs it means is
    /// carrying every record it walked; reading them back by id would open the
    /// whole store a second time for one answer.
    pub(crate) preloaded: Option<Vec<Arc<RunMeta>>>,
    pub(crate) digest: String,
}

impl RunSpec {
    /// Does any requested source read files?
    pub(crate) fn searches_filesystem(&self) -> bool {
        self.q.is_some() && self.sources.iter().any(|s| s.reads_filesystem())
    }
}

/// Order by `(sort value, run_id)`, with the tie-break following the primary
/// direction.
///
/// The tie-break is not decoration: two runs can start in the same second, and
/// a keyset walk over a non-total order drops whichever colliding run it
/// resumed past. Run ids are unique, so this makes the order total.
pub(crate) fn sort_runs(runs: &mut [Arc<RunMeta>], spec: &RunSpec) {
    runs.sort_by(|a, b| {
        let ka = (spec.sort.key(a), a.run_id.as_str());
        let kb = (spec.sort.key(b), b.run_id.as_str());
        if spec.descending {
            kb.cmp(&ka)
        } else {
            ka.cmp(&kb)
        }
    });
}

/// Take this page's runs and mint the cursor for the next one.
///
/// Takes `limit + 1` and keeps `limit`, so a cursor is only ever emitted when a
/// further item is known to exist. Emitting one speculatively would make a
/// client's "loop until null" run one empty request longer, every time.
pub(crate) fn paginate(
    runs: Vec<Arc<RunMeta>>,
    spec: &RunSpec,
) -> (Vec<Arc<RunMeta>>, Option<String>) {
    let mut after_cursor: Vec<Arc<RunMeta>> = match spec.cursor {
        None => runs,
        Some(ref cursor) => runs
            .into_iter()
            .filter(|meta| cursor.precedes(&spec.sort.key(meta), &meta.run_id, spec.descending))
            .collect(),
    };

    let has_more = after_cursor.len() > spec.limit;
    after_cursor.truncate(spec.limit);
    let next = has_more.then(|| after_cursor.last()).flatten().map(|last| {
        cursor::encode(
            spec.sort.as_str(),
            if spec.descending { "desc" } else { "asc" },
            &spec.digest,
            spec.sort.key(last),
            &last.run_id,
        )
    });
    (after_cursor, next)
}

/// One run in a listing, with why it matched.
///
/// The meta is shared rather than cloned: the run index already holds it
/// behind an `Arc`, and a page of fifty is fifty pointer copies.
pub(crate) struct RunHit {
    /// The run.
    pub(crate) meta: Arc<RunMeta>,
    /// Why it matched the search, empty when there was no `q`.
    pub(crate) highlights: Vec<Highlight>,
}

/// One page of runs, before either surface renders it.
pub(crate) struct RunListing {
    /// The runs on this page, in the requested order.
    pub(crate) hits: Vec<RunHit>,
    /// Cursor for the page after this one, absent when this is the last.
    pub(crate) next_cursor: Option<String>,
    /// How many runs matched, absent when the scan was cut short.
    pub(crate) total: Option<usize>,
    /// Whether the filesystem-reading search gave up before covering the set.
    pub(crate) scan_truncated: bool,
    /// Ids from a batch fetch that name no run on this machine.
    pub(crate) missing: Vec<String>,
    /// Daemon time when the page was built, for polling with `since`.
    pub(crate) server_time: i64,
}

/// The runs a batch fetch names, read straight from their own records.
///
/// Unpaged and in the order the ids were given: the caller already said which
/// runs it wants and how many, so there is nothing left for a sort or a cursor
/// to decide. Ids that name no run on this machine are reported rather than
/// thrown, so one dead id costs a client nothing else in the batch.
fn read_by_id(ids: &[String]) -> (Vec<Arc<RunMeta>>, Vec<String>) {
    let mut found: Vec<Arc<RunMeta>> = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        noted(id);
        match runstate::read_meta(id) {
            Ok(meta) => found.push(Arc::new(meta)),
            Err(_) => missing.push(id.clone()),
        }
    }
    (found, missing)
}

/// What a read of one run's record straight from disk is reported to.
///
/// A caller that walked the store already holds every record it walked, and
/// opening each one again is the store read twice for one answer. That is only
/// visible as work that did not happen, so the reads report here and a test
/// counts them.
pub(crate) type RecordReadRecorder = Box<dyn Fn(&str) + Send + Sync>;

/// Where a record read is reported, when anything is listening.
///
/// Nothing installs a recorder in a server: the list would grow for ever and
/// nothing but a test has any use for it, so a read costs a load of this and a
/// call it does not make.
static RECORDER: std::sync::OnceLock<RecordReadRecorder> = std::sync::OnceLock::new();

/// Report every record read to `record`, for as long as this process lives.
///
/// Once, deliberately: a second call is a no-op, so each test that wants the
/// log can ask for it rather than arranging to be the one that installs it.
#[cfg(test)]
pub(crate) fn record_record_reads(record: RecordReadRecorder) {
    drop(RECORDER.set(record));
}

/// Note that one run's record is about to be opened.
fn noted(run_id: &str) {
    if let Some(record) = RECORDER.get() {
        record(run_id);
    }
}

/// Answer a batch fetch by id.
///
/// `held` is the records a caller that already walked the store is carrying.
/// The ids came out of those records, so reading them back would open every
/// one of them a second time for the same answer.
fn by_ids(ids: &[String], held: Option<&[Arc<RunMeta>]>, server_time: i64) -> RunListing {
    let (found, missing) = match held {
        Some(held) => (held.to_vec(), Vec::new()),
        None => read_by_id(ids),
    };
    let total = found.len();
    RunListing {
        hits: found
            .into_iter()
            .map(|meta| RunHit {
                meta,
                highlights: Vec::new(),
            })
            .collect(),
        next_cursor: None,
        total: Some(total),
        scan_truncated: false,
        missing,
        server_time,
    }
}

/// Answer a listing request.
///
/// A batch fetch by id reads exactly the runs it names. Everything else walks
/// the shared index: filter, sort, search, then page. The order matters and is
/// not free to change: filtering before `total` makes the count describe what
/// was asked for, and sorting before searching spends the scan budget on the
/// runs the client asked to see first.
pub(crate) async fn list(state: &AppState, spec: &RunSpec) -> RunListing {
    let server_time = leviath_core::duration::now_secs();

    if let Some(ref ids) = spec.ids {
        return by_ids(ids, spec.preloaded.as_deref(), server_time);
    }

    let snapshot = state.caches.run_index.snapshot().await;
    // A subtree is the one filter that needs the tree rather than the record, so
    // it is walked once here from the index's own parent map rather than per run.
    let descendants = match &spec.parent {
        ParentFilter::Under(root) => snapshot.descendants_of(root),
        _ => HashSet::new(),
    };
    let mut runs = snapshot.into_runs();
    // Before the sort and before `total`, like every other filter here, so the
    // count describes what was asked for rather than what is on the machine.
    runs.retain(|meta| spec.parent.keeps_in(meta, &descendants));
    if let Some(ref blueprint) = spec.blueprint {
        runs.retain(|meta| &meta.agent_name == blueprint);
    }
    if !spec.statuses.is_empty() {
        runs.retain(|meta| {
            spec.statuses
                .iter()
                .any(|filter| status_matches(&meta.status, filter))
        });
    }
    if let Some(since) = spec.since {
        // Inclusive: at seconds granularity an exclusive comparison drops
        // updates that land in the same second as the previous watermark, and a
        // re-delivered item is recoverable where a lost one is not. Compared as
        // the key rather than as a number, so it says nothing about an order
        // this route cannot be asked for.
        runs.retain(|meta| spec.sort.key(meta) >= CursorKey::Int(since));
    }

    // Sort before searching, so the scan budget is spent on the runs the client
    // asked to see first.
    sort_runs(&mut runs, spec);

    let (runs, scan_truncated) = apply_search(runs, spec);
    // Null when the scan was cut short: a count taken from a partial scan is
    // worse than no count, because a UI renders it as fact.
    let total = (!scan_truncated).then_some(runs.len());

    let (page_runs, next_cursor) = paginate(runs, spec);
    let hits = page_runs
        .into_iter()
        .map(|meta| {
            let highlights = spec
                .q
                .as_deref()
                .map(|q| highlights_for(&meta, q, &spec.sources))
                .unwrap_or_default();
            RunHit { meta, highlights }
        })
        .collect();

    RunListing {
        hits,
        next_cursor,
        total,
        scan_truncated,
        missing: Vec::new(),
        server_time,
    }
}

/// Answer a listing request lazily.
///
/// The counterpart of [`list`], and deliberately not a replacement for it:
/// `list` keeps its search budget, its `scan_truncated` and its batch fetch by
/// id, all of which `GET /api/runs` promises. This one has no cap of any kind,
/// and opens nothing for a run the page it was asked for does not reach - so a
/// filter that has to read files costs what that page costs rather than what
/// the store costs.
///
/// The five steps are [`paging::walk`](crate::commands::serve::graphql::paging::walk),
/// shared with every other listing; [`lazy::RunSift`] is what a run means by
/// each of them.
/// A listing that names its runs by id reads exactly those records rather than
/// the index, which is what keeps "these five runs" one read per run however
/// large the store is. Everything the filter says still applies to what comes
/// back.
pub(crate) async fn walk(
    state: &AppState,
    spec: &RunSpec,
) -> super::super::graphql::paging::walk::Page<lazy::RunSift> {
    let (sift, items) = prepared(state, spec).await;
    super::super::graphql::paging::walk::walk(sift, items, spec.cursor.as_ref(), spec.limit).await
}

/// Every run the filter matches, with no page to stop at.
///
/// For a bulk mutation and an export, whose answer is a set rather than a slice
/// of one. The same five steps run, reads included, so a run this hands back is
/// exactly a run a page would have listed; what it skips is the cursor and the
/// page size, neither of which a caller that wants everything has anything to
/// say about.
pub(crate) async fn walk_all(state: &AppState, spec: &RunSpec) -> Vec<Arc<RunMeta>> {
    let (sift, items) = prepared(state, spec).await;
    // The page size is the whole snapshot, so the walk stops where the runs do.
    let whole = items.len();
    super::super::graphql::paging::walk::walk(sift, items, None, whole)
        .await
        .items
}

/// What a run walk starts from: the sift it consults, and the runs it walks.
async fn prepared(state: &AppState, spec: &RunSpec) -> (lazy::RunSift, Vec<Arc<RunMeta>>) {
    let snapshot = state.caches.run_index.snapshot().await;
    // A subtree is the one filter that needs the tree rather than the record,
    // so it is resolved once here from the index's own parent map.
    let descendants = match &spec.parent {
        ParentFilter::Under(root) => snapshot.descendants_of(root),
        _ => HashSet::new(),
    };
    let indexed = snapshot.into_runs();
    // Linked once for the whole walk, and only where a filter can ask: a
    // listing with no predicate never looks above the run in front of it.
    let now = leviath_core::duration::now_secs();
    let ctx = match spec.predicate {
        None => MatchContext::at(now),
        Some(_) => MatchContext {
            now,
            tree: Arc::new(RunTree::of(&indexed)),
        },
    };
    let items = match spec.ids {
        None => indexed,
        Some(ref ids) => read_by_id(ids).0,
    };
    (lazy::RunSift::new(spec, descendants, ctx), items)
}

/// What a listing asks for, before the cursor is checked against it.
///
/// One of these is what each surface builds: `GET /api/runs` from query
/// parameters, GraphQL's `runs` field from its arguments. Turning it into a
/// [`RunSpec`] computes the filter digest and decodes the cursor against it,
/// which is the step that makes a cursor from one filter set unusable with
/// another.
#[derive(Debug)]
pub(crate) struct RunSelection {
    /// Page size, already bounded by the caller.
    pub(crate) limit: usize,
    /// Only runs of this blueprint, by the name the run recorded. A name
    /// nothing matches gives an empty page rather than an error: a blueprint
    /// with no runs yet is an ordinary answer.
    pub(crate) blueprint: Option<String>,
    /// Status filters, in the daemon's own spelling.
    pub(crate) statuses: Vec<String>,
    /// Which timestamp orders the listing, and breaks a tie in a listing
    /// ordered by several.
    pub(crate) sort: SortKey,
    /// Newest first when true.
    pub(crate) descending: bool,
    /// Every sort key, in priority order, for a surface that offers more than
    /// one.
    ///
    /// Absent is the single-key order `sort` and `descending` spell, which is
    /// what `GET /api/runs` asks for and what mints the cursor both surfaces
    /// interchange.
    pub(crate) order: Option<Vec<Term<SortKey>>>,
    /// The search text, when there is one.
    pub(crate) q: Option<String>,
    /// Where the search looks.
    pub(crate) sources: Vec<Source>,
    /// The sources exactly as the request spelled them, for the digest.
    ///
    /// The parsed list would digest the same for `logs,meta` and `meta,logs`,
    /// and a cursor is a promise about one walk, not about a set that happens
    /// to compare equal.
    pub(crate) sources_raw: String,
    /// Which top-level fields a REST projection keeps; GraphQL leaves it None
    /// and uses its selection set instead.
    pub(crate) fields: Option<HashSet<String>>,
    /// Exact ids for a batch fetch, which is not a filter.
    pub(crate) ids: Option<Vec<String>>,
    /// Inclusive lower bound on the sort value.
    pub(crate) since: Option<i64>,
    /// Which runs the listing is about.
    pub(crate) parent: ParentFilter,
    /// A composable predicate, for a surface that builds one.
    ///
    /// `GET /api/runs` leaves it absent and filters with the fields above;
    /// GraphQL's `runs` field puts its whole filter tree here.
    pub(crate) predicate: Option<Arc<dyn RunPredicate>>,
    /// The records behind `ids`, where the caller already holds them.
    ///
    /// Set by a caller that walked the store to work out which runs it means,
    /// so the listing hands those records back instead of opening every one of
    /// them again.
    pub(crate) preloaded: Option<Vec<Arc<RunMeta>>>,
}

impl RunSelection {
    /// Digest the filters, decode the cursor against them, and produce the
    /// spec the listing runs on.
    ///
    /// A cursor that was minted for a different filter set, sort or order is
    /// refused here rather than silently resuming a walk of something else.
    pub(crate) fn resolve(
        self,
        raw_cursor: Option<&str>,
    ) -> Result<RunSpec, super::error::ServeError> {
        let mut spec = self.unpaged();
        spec.cursor = match raw_cursor {
            None => None,
            Some(raw) => Some(spec.order.decode(raw, &spec.digest)?),
        };
        Ok(spec)
    }

    /// The same spec with no page to resume from.
    ///
    /// Separate from [`resolve`](Self::resolve) because a caller that walks
    /// everything has no cursor to refuse: an export and a bulk act read the
    /// whole selection, and a refusal neither of them can reach is a branch
    /// nobody can read the meaning of.
    pub(crate) fn unpaged(self) -> RunSpec {
        // The filters, in a fixed order, so the same filter set always digests
        // the same way.
        let mut parts = vec![
            self.statuses.join(","),
            self.q.clone().unwrap_or_default(),
            self.sources_raw.clone(),
            self.since.map(|s| s.to_string()).unwrap_or_default(),
        ];
        // Appended only when it filters something. A digest identifies the
        // filter *set*, and `Any` is the absence of this one - so a listing
        // that does not use it digests exactly as it did before the parameter
        // existed, and every cursor a client is already holding stays valid
        // across the upgrade.
        if let Some(part) = self.parent.digest_part() {
            parts.push(part);
        }
        // Appended only when it filters, for the same reason: a digest
        // identifies the filter set, and an absent filter has to digest as
        // absent so a cursor minted before this existed still resumes.
        if let Some(ref blueprint) = self.blueprint {
            parts.push(format!("blueprint:{blueprint}"));
        }
        // Appended only when there is one, for the same reason: a surface that
        // builds no predicate digests exactly as it would without this, so a
        // cursor either surface minted resumes on the other.
        if let Some(ref predicate) = self.predicate {
            parts.push(format!("predicate:{}", predicate.digest_part()));
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let digest = cursor::filter_digest(&refs);

        // A single-term order records the bare field name and direction, which
        // is byte for byte what this route has always minted, so a cursor from
        // either surface still resumes on the other.
        let direction = match self.descending {
            true => OrderDirection::Desc,
            false => OrderDirection::Asc,
        };
        let order = Order::new(self.order.unwrap_or_else(|| {
            vec![Term {
                field: self.sort,
                direction,
            }]
        }));
        RunSpec {
            limit: self.limit,
            cursor: None,
            statuses: self.statuses,
            sort: self.sort,
            descending: self.descending,
            order,
            q: self.q,
            sources: self.sources,
            fields: self.fields,
            ids: self.ids,
            since: self.since,
            parent: self.parent,
            blueprint: self.blueprint,
            predicate: self.predicate,
            preloaded: self.preloaded,
            digest,
        }
    }
}

/// Whether a run may be removed, or the reason it may not.
///
/// One definition for the single and bulk routes, so a run that 409s on its own
/// cannot be silently deleted as part of a sweep.
///
/// `force` covers only the last case below, and only the single-run route ever
/// passes it.
pub(crate) fn deletable(id: &str, force: bool) -> Result<(), super::error::ServeError> {
    let dir = runstate::run_dir(id);
    if !dir.exists() {
        return Err(super::error::ServeError::NotFound(format!(
            "Run '{id}' not found"
        )));
    }
    // Judged from the run's own record, not by asking the daemon: a daemon that
    // is down must not make every run undeletable.
    match runstate::read_meta(id) {
        Ok(meta) if runstate::is_terminal_status(&meta.status) => Ok(()),
        Ok(meta) => Err(super::error::ServeError::Conflict(format!(
            "Run '{id}' is {}; cancel it before deleting it",
            meta.status
        ))),
        // A run whose `meta.json` will not parse says nothing about whether it
        // is finished, and "cannot read it" must not quietly read as "finished".
        // An unparseable record is what a *live* run looks like to a binary
        // whose `RunMeta` has moved on, and the failure mode there is deleting a
        // running agent's directory and answering 204 - which is precisely what
        // this route refuses to do for a run it *can* see is live.
        //
        // Such a run is still skipped by `list_runs`, which would leave it both
        // invisible and permanent, so the escape hatch stays - as something the
        // caller types rather than something that happens to them.
        Err(_) if force => Ok(()),
        Err(e) => Err(super::error::ServeError::Conflict(format!(
            "Run '{id}' has no readable record ({e}), so it cannot be shown \
             to be finished; pass force=true to delete it anyway"
        ))),
    }
}

/// Remove a run's directory, having already decided it may go.
pub(crate) fn remove_run(id: &str) -> Result<(), super::error::ServeError> {
    runstate::forget_provider_files(id);
    std::fs::remove_dir_all(runstate::run_dir(id)).map_err(|e| {
        super::error::ServeError::Internal(format!("Failed to delete run '{id}': {e}"))
    })
}

/// Whether every member of `ids` may go, or the reason one of them may not.
///
/// A live sub-agent blocks the whole delete rather than being skipped: half a
/// tree is not a state anything downstream knows how to read, and removing the
/// parent of a running agent is exactly what [`deletable`] refuses to do for
/// the run named directly. The reason names the sub-agent, because "cancel it
/// before deleting it" about a run the caller never mentioned is unactionable.
pub(crate) fn deletable_family(
    root: &str,
    ids: &[String],
    force: bool,
) -> Result<(), super::error::ServeError> {
    for id in ids {
        deletable(id, force).map_err(|failure| match id == root {
            true => failure,
            // The same refusal, of the same kind, saying which sub-agent it is
            // about: "cancel it before deleting it" of a run the caller never
            // mentioned is not something anybody can act on.
            false => failure.with_context(&format!(
                "It is a sub-agent run of '{root}', deleted with it"
            )),
        })?;
    }
    Ok(())
}

/// Remove every run in `ids`, stopping at the first failure.
pub(crate) fn remove_family(ids: &[String]) -> Result<(), super::error::ServeError> {
    for id in ids {
        remove_run(id)?;
    }
    Ok(())
}

/// One id a bulk delete passed over, and why.
pub(crate) struct SkippedDelete {
    /// The run that stayed.
    pub(crate) id: String,
    /// Why it did.
    pub(crate) reason: String,
}

/// What a bulk delete removed, and what it left.
///
/// Partial success is the normal outcome, not an edge case: a sweep names runs
/// by a predicate, and one of them being live or unreadable is not a reason to
/// refuse the rest.
pub(crate) struct DeleteOutcome {
    /// The runs that were removed, sub-agents included.
    pub(crate) deleted: Vec<String>,
    /// The ones that stayed, each with its reason.
    pub(crate) skipped: Vec<SkippedDelete>,
}

/// Which runs a delete is about.
pub(crate) enum DeleteTargets {
    /// Exactly these, named by the caller.
    Ids(Vec<String>),
    /// Every finished run last touched before this second.
    Before(i64),
}

/// Delete run records.
///
/// Deleting a run takes its sub-agents with it: their records are only
/// meaningful under the run that started them, and leaving them behind is how a
/// listing fills with workers whose parent is gone.
pub(crate) async fn delete(
    state: &AppState,
    targets: DeleteTargets,
    force: bool,
) -> Result<DeleteOutcome, super::error::ServeError> {
    let ids = match targets {
        DeleteTargets::Ids(ids) => {
            if ids.len() > MAX_IDS {
                return Err(super::error::ServeError::BadRequest(format!(
                    "`ids` names {} runs; at most {MAX_IDS} may be deleted at once",
                    ids.len()
                )));
            }
            ids
        }
        // Scoped to finished runs at selection time as well as in the check
        // below, so a sweep does not report every live run on the machine as
        // skipped.
        DeleteTargets::Before(before) => state
            .caches
            .run_index
            .snapshot()
            .await
            .into_runs()
            .into_iter()
            .filter(|meta| runstate::is_terminal_status(&meta.status) && meta.updated_at < before)
            .map(|meta| meta.run_id.clone())
            .collect(),
    };

    let mut deleted: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        // A sweep selects a parent and its children independently, and naming a
        // parent already took its children; either way the second mention is of
        // a run this request just removed, which is a deletion rather than the
        // "no such run" the check would report.
        if deleted.contains(&id) {
            continue;
        }
        let family = runstate::family_of(&id);
        match deletable_family(&id, &family, force).and_then(|()| remove_family(&family)) {
            Ok(()) => deleted.extend(family),
            Err(reason) => skipped.push(SkippedDelete {
                id,
                reason: reason.to_string(),
            }),
        }
    }
    Ok(DeleteOutcome { deleted, skipped })
}
