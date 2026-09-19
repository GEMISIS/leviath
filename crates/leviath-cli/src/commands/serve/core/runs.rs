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
use super::super::search;
use super::super::types::{AppState, Highlight, status_matches};
use crate::runstate::{self, RunMeta};

pub(crate) mod matching;
use matching::*;

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
/// The shared `At` suffix is the point, not an accident: these are the three
/// timestamps on a run, and each variant is named for the `RunMeta` field it
/// reads and the query value that selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortKey {
    Started,
    Updated,
    LastProgress,
}

impl SortKey {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "started_at" => Some(SortKey::Started),
            "updated_at" => Some(SortKey::Updated),
            "last_progress_at" => Some(SortKey::LastProgress),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SortKey::Started => "started_at",
            SortKey::Updated => "updated_at",
            SortKey::LastProgress => "last_progress_at",
        }
    }

    /// This run's value for the key.
    ///
    /// `last_progress_at` is `Option`, and absent means "written by a daemon
    /// older than the field, or before the first snapshot landed". The run
    /// demonstrably started, so `started_at` is the honest floor - and it keeps
    /// the key non-null, which the cursor needs.
    pub(crate) fn value(self, meta: &RunMeta) -> i64 {
        match self {
            SortKey::Started => meta.started_at,
            SortKey::Updated => meta.updated_at,
            SortKey::LastProgress => meta.last_progress_at.unwrap_or(meta.started_at),
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
            Some(id) => Self::Of(id.to_string()),
        }
    }

    /// Whether this run belongs in the listing.
    pub(crate) fn keeps(&self, meta: &RunMeta) -> bool {
        match self {
            Self::Any => true,
            Self::Roots => meta.parent_run_id.is_none(),
            Self::Of(parent) => meta.parent_run_id.as_deref() == Some(parent.as_str()),
        }
    }

    /// This filter's contribution to the cursor digest, so a walk cannot change
    /// what it is filtering halfway through.
    ///
    /// `None` for [`Any`](Self::Any), which contributes nothing at all rather
    /// than an empty part - an empty part is still a part, and would have
    /// changed the digest of every unfiltered listing and so invalidated every
    /// cursor a client was holding when it upgraded.
    pub(crate) fn digest_part(&self) -> Option<&str> {
        match self {
            Self::Any => None,
            Self::Roots => Some("none"),
            Self::Of(parent) => Some(parent.as_str()),
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
    pub(crate) q: Option<String>,
    pub(crate) sources: Vec<Source>,
    pub(crate) fields: Option<HashSet<String>>,
    pub(crate) ids: Option<Vec<String>>,
    pub(crate) since: Option<i64>,
    pub(crate) parent: ParentFilter,
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
        let ka = (spec.sort.value(a), a.run_id.as_str());
        let kb = (spec.sort.value(b), b.run_id.as_str());
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
            .filter(|meta| {
                cursor.precedes(
                    &CursorKey::Int(spec.sort.value(meta)),
                    &meta.run_id,
                    spec.descending,
                )
            })
            .collect(),
    };

    let has_more = after_cursor.len() > spec.limit;
    after_cursor.truncate(spec.limit);
    let next = has_more.then(|| after_cursor.last()).flatten().map(|last| {
        cursor::encode(
            spec.sort.as_str(),
            if spec.descending { "desc" } else { "asc" },
            &spec.digest,
            CursorKey::Int(spec.sort.value(last)),
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
        let mut hits = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            match runstate::read_meta(id) {
                Ok(meta) => hits.push(RunHit {
                    meta: Arc::new(meta),
                    highlights: Vec::new(),
                }),
                Err(_) => missing.push(id.clone()),
            }
        }
        let total = hits.len();
        return RunListing {
            hits,
            next_cursor: None,
            total: Some(total),
            scan_truncated: false,
            missing,
            server_time,
        };
    }

    let mut runs = state.caches.run_index.snapshot().await.into_runs();
    // Before the sort and before `total`, like every other filter here, so the
    // count describes what was asked for rather than what is on the machine.
    runs.retain(|meta| spec.parent.keeps(meta));
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
        // re-delivered item is recoverable where a lost one is not.
        runs.retain(|meta| spec.sort.value(meta) >= since);
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
    /// Status filters, in the daemon's own spelling.
    pub(crate) statuses: Vec<String>,
    /// Which timestamp orders the listing.
    pub(crate) sort: SortKey,
    /// Newest first when true.
    pub(crate) descending: bool,
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
            parts.push(part.to_string());
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let digest = cursor::filter_digest(&refs);

        let order_raw = if self.descending { "desc" } else { "asc" };
        let cursor = match raw_cursor {
            None => None,
            Some(raw) => Some(
                cursor::decode(raw, self.sort.as_str(), order_raw, &digest)
                    .map_err(|e| super::error::ServeError::BadRequest(e.message()))?,
            ),
        };

        Ok(RunSpec {
            limit: self.limit,
            cursor,
            statuses: self.statuses,
            sort: self.sort,
            descending: self.descending,
            q: self.q,
            sources: self.sources,
            fields: self.fields,
            ids: self.ids,
            since: self.since,
            parent: self.parent,
            digest,
        })
    }
}
