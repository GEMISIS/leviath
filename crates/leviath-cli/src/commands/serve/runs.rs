//! `GET /api/runs` - the paginated, searchable run listing.
//!
//! Supersedes `GET /api/agents`, which returns every run ever recorded as one
//! unbounded array and accepts only a status filter. That route stays exactly as
//! it is, deprecated: it is the legacy spelling (the console says "runs"
//! everywhere), and it gets a replacement at a new path rather than a changed
//! response shape, so nothing that calls it today breaks.
//!
//! What this adds over that: keyset pagination, sorting, server-side search with
//! highlights, batch fetch by id, and field projection.
//!
//! **What it does not fix.** Every listing here still walks the runs directory
//! and parses every `meta.json`, because that is the only index there is.
//! Pagination bounds what crosses the wire and what the browser holds; it does
//! not bound the server's work. The guard that does bound the damage is
//! [`MAX_SEARCH_SCAN`], on the filesystem-reading half of search.
//!
//! Pruning is [`delete_run`] and [`delete_runs`], which is the other half of
//! that story: the listing can now be made smaller, not only paged over.

use std::collections::HashSet;

use axum::extract::{Path as AxumPath, Query};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::cursor::{self, Cursor, CursorKey};
use super::search;
use super::types::*;
use crate::runstate::{self, RunMeta};

/// Page size when the client does not ask.
const DEFAULT_LIMIT: usize = 50;
/// Largest page size served. A larger `limit` is clamped rather than refused: a
/// client asking for 1000 wants as much as it can get, and the real value is
/// discoverable from `GET /api/config`.
pub(super) const MAX_LIMIT: usize = 200;
/// Most ids one batch fetch may name.
pub(super) const MAX_IDS: usize = 200;
/// How many runs a filesystem-reading search will examine before giving up.
///
/// `q_in=logs` over an unbounded, never-pruned run set is a self-inflicted
/// denial of service: every request would read two files per stage per run, for
/// every run that has ever existed. Stopping after a bounded prefix - taken in
/// the requested sort order, so it is the newest runs - answers the common case
/// and says so via `scan_truncated`, which is better than refusing the query or
/// than quietly taking longer every month.
pub(super) const MAX_SEARCH_SCAN: usize = 500;
/// How much of each stage log a search reads, from the end.
pub(super) const SEARCH_LOG_TAIL_BYTES: u64 = 256 * 1024;
/// Most highlights attached to one item. A log with ten thousand matches must
/// not become the response body.
const MAX_HIGHLIGHTS: usize = 5;

/// Which field a run is ordered by.
///
/// The shared `At` suffix is the point, not an accident: these are the three
/// timestamps on a run, and each variant is named for the `RunMeta` field it
/// reads and the query value that selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Started,
    Updated,
    LastProgress,
}

impl SortKey {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "started_at" => Some(SortKey::Started),
            "updated_at" => Some(SortKey::Updated),
            "last_progress_at" => Some(SortKey::LastProgress),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
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
    fn value(self, meta: &RunMeta) -> i64 {
        match self {
            SortKey::Started => meta.started_at,
            SortKey::Updated => meta.updated_at,
            SortKey::LastProgress => meta.last_progress_at.unwrap_or(meta.started_at),
        }
    }
}

/// Where search looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
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
    fn parse(raw: &str) -> Option<Self> {
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
    fn reads_filesystem(self) -> bool {
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
enum ParentFilter {
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
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => Self::Any,
            Some("none") => Self::Roots,
            Some(id) => Self::Of(id.to_string()),
        }
    }

    /// Whether this run belongs in the listing.
    fn keeps(&self, meta: &RunMeta) -> bool {
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
    fn digest_part(&self) -> Option<&str> {
        match self {
            Self::Any => None,
            Self::Roots => Some("none"),
            Self::Of(parent) => Some(parent.as_str()),
        }
    }
}

/// Query parameters of `GET /api/runs`.
#[derive(serde::Deserialize, Default)]
pub(super) struct RunsQuery {
    pub(super) limit: Option<usize>,
    pub(super) cursor: Option<String>,
    pub(super) status: Option<String>,
    pub(super) sort: Option<String>,
    pub(super) order: Option<String>,
    pub(super) q: Option<String>,
    pub(super) q_in: Option<String>,
    pub(super) fields: Option<String>,
    pub(super) ids: Option<String>,
    pub(super) since: Option<i64>,
    pub(super) parent: Option<String>,
}

/// A validated query. Every 400 this route can produce is decided here, so the
/// handler below is a straight-line composition and the error paths are all
/// reachable from a plain unit test.
struct Resolved {
    limit: usize,
    cursor: Option<Cursor>,
    statuses: Vec<String>,
    sort: SortKey,
    descending: bool,
    q: Option<String>,
    sources: Vec<Source>,
    fields: Option<HashSet<String>>,
    ids: Option<Vec<String>>,
    since: Option<i64>,
    parent: ParentFilter,
    digest: String,
}

impl Resolved {
    /// Does any requested source read files?
    fn searches_filesystem(&self) -> bool {
        self.q.is_some() && self.sources.iter().any(|s| s.reads_filesystem())
    }
}

type ApiError = (StatusCode, Json<ErrorResponse>);

fn bad_request(message: String) -> ApiError {
    err(StatusCode::BAD_REQUEST, message)
}

/// Split a comma list, dropping empties so `a,,b` and a trailing comma are not
/// errors a client has to think about.
fn comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn resolve(query: &RunsQuery) -> Result<Resolved, ApiError> {
    // `ids` is a batch fetch, not a filter: it names exactly what it wants, so
    // paging, ordering and filtering have nothing to act on. Rejecting the
    // combination is deliberate - a silently ignored parameter produces the
    // kind of bug report that takes a day to read.
    let parent = ParentFilter::parse(query.parent.as_deref());
    let ids = query.ids.as_deref().map(comma_list);
    if let Some(ref ids) = ids {
        let conflicts = [
            ("cursor", query.cursor.is_some()),
            ("q", query.q.is_some()),
            ("status", query.status.is_some()),
            ("since", query.since.is_some()),
            // The resolved filter rather than the raw parameter, so `parent=`
            // is the no-op it looks like rather than a conflict.
            ("parent", parent != ParentFilter::Any),
        ];
        if let Some((name, _)) = conflicts.iter().find(|(_, present)| *present) {
            return Err(bad_request(format!(
                "`ids` names exactly which runs to return, so it cannot be combined with `{name}`"
            )));
        }
        if ids.len() > MAX_IDS {
            return Err(bad_request(format!(
                "`ids` names {} runs; at most {MAX_IDS} may be fetched at once",
                ids.len()
            )));
        }
    }

    let limit = match query.limit {
        None => DEFAULT_LIMIT,
        Some(0) => {
            return Err(bad_request(
                "`limit` must be at least 1; omit it for the default".to_string(),
            ));
        }
        Some(n) => n.min(MAX_LIMIT),
    };

    let sort_raw = query.sort.as_deref().unwrap_or("started_at");
    let sort = SortKey::parse(sort_raw).ok_or_else(|| {
        bad_request(format!(
            "Unknown sort '{sort_raw}': expected started_at, updated_at or last_progress_at"
        ))
    })?;

    let order_raw = query.order.as_deref().unwrap_or("desc");
    let descending = match order_raw {
        "desc" => true,
        "asc" => false,
        other => {
            return Err(bad_request(format!(
                "Unknown order '{other}': expected desc or asc"
            )));
        }
    };

    let q = query
        .q
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let sources_raw = query.q_in.as_deref().unwrap_or("meta,files");
    let mut sources = Vec::new();
    for name in comma_list(sources_raw) {
        let source = Source::parse(&name).ok_or_else(|| {
            bad_request(format!(
                "Unknown q_in '{name}': expected meta, files, context, logs or journal"
            ))
        })?;
        if !sources.contains(&source) {
            sources.push(source);
        }
    }

    let fields = match query.fields.as_deref() {
        None => None,
        Some(raw) => {
            let requested = comma_list(raw);
            let known = known_meta_fields();
            let unknown: Vec<&String> = requested
                .iter()
                .filter(|name| !known.contains(name.as_str()))
                .collect();
            if let Some(first) = unknown.first() {
                // Naming the nested case separately, because `flags.count` is
                // the natural thing to try and "unknown field" would be a
                // misleading answer to it.
                if first.contains('.') {
                    return Err(bad_request(format!(
                        "`fields` selects top-level fields only, so '{first}' is not available"
                    )));
                }
                return Err(bad_request(format!("Unknown field '{first}' in `fields`")));
            }
            let mut set: HashSet<String> = requested.into_iter().collect();
            // Identity is never optional: a projected item nothing can be keyed
            // by is useless to every client.
            set.insert("run_id".to_string());
            Some(set)
        }
    };

    let statuses = query.status.as_deref().map(comma_list).unwrap_or_default();

    // The filters, in a fixed order, so the same filter set always digests the
    // same way.
    let since_part = query.since.map(|s| s.to_string()).unwrap_or_default();
    let mut parts = vec![
        statuses.join(","),
        q.clone().unwrap_or_default(),
        sources_raw.to_string(),
        since_part,
    ];
    // Appended only when it filters something. A digest identifies the filter
    // *set*, and `Any` is the absence of this one - so a listing that does not
    // use it digests exactly as it did before the parameter existed, and every
    // cursor a client is already holding stays valid across the upgrade.
    if let Some(part) = parent.digest_part() {
        parts.push(part.to_string());
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let digest = cursor::filter_digest(&refs);

    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => Some(
            cursor::decode(raw, sort.as_str(), order_raw, &digest)
                .map_err(|e| bad_request(e.message()))?,
        ),
    };

    Ok(Resolved {
        limit,
        cursor,
        statuses,
        sort,
        descending,
        q,
        sources,
        fields,
        ids,
        since: query.since,
        parent,
        digest,
    })
}

/// The top-level keys of a serialized `RunMeta`, for validating `fields`.
///
/// Derived from an actual serialization rather than a hand-written list, so the
/// allowlist cannot drift away from the struct when a field is added.
///
/// Every `Option` field is filled first. Several carry
/// `skip_serializing_if = "Option::is_none"`, so a probe left at its defaults
/// omits them and the allowlist silently refuses a field that does exist -
/// `?fields=read_paths` and `?fields=final_output` were both rejected on runs
/// that had them. Filling the options is what makes the sentence above true.
fn known_meta_fields() -> HashSet<String> {
    let mut probe = RunMeta::new(
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        None,
        String::new(),
        0,
    );
    probe.read_paths = Some(Default::default());
    probe.final_output = Some(Default::default());
    probe.output_request = Some(Default::default());
    // `RunMeta` is a struct, so this is always an object; `as_object` keeps
    // that assumption in one place instead of adding a match arm nothing can
    // reach.
    let mut fields: HashSet<String> = serde_json::to_value(probe)
        .ok()
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    // Not on the struct, but on every item this route serves - see
    // `build_item`. Left out, `?fields=working_secs` would be refused for a key
    // the response carries.
    fields.insert(leviath_core::duration::AGE_SECS_KEY.to_string());
    fields.insert(leviath_core::duration::WORKING_SECS_KEY.to_string());
    fields
}

/// `GET /api/runs`
pub(super) async fn list_runs(
    Query(query): Query<RunsQuery>,
) -> Result<Json<Page<RunItem>>, ApiError> {
    let resolved = resolve(&query)?;
    let server_time = leviath_core::duration::now_secs();

    // A batch fetch reads exactly the named runs, rather than scanning the
    // whole directory and filtering it down to them.
    if let Some(ref ids) = resolved.ids {
        let mut items = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            match runstate::read_meta(id) {
                Ok(meta) => items.push(build_item(&meta, &resolved, None)),
                Err(_) => missing.push(id.clone()),
            }
        }
        let total = items.len();
        let mut page = Page::new(items, None, Some(total), server_time);
        page.missing = missing;
        return Ok(Json(page));
    }

    let mut runs = runstate::list_runs();
    // Before the sort and before `total`, like every other filter here, so the
    // count describes what was asked for rather than what is on the machine.
    runs.retain(|meta| resolved.parent.keeps(meta));
    if !resolved.statuses.is_empty() {
        runs.retain(|meta| {
            resolved
                .statuses
                .iter()
                .any(|filter| status_matches(&meta.status, filter))
        });
    }
    if let Some(since) = resolved.since {
        // Inclusive: at seconds granularity an exclusive comparison drops
        // updates that land in the same second as the previous watermark, and a
        // re-delivered item is recoverable where a lost one is not.
        runs.retain(|meta| resolved.sort.value(meta) >= since);
    }

    // Sort before searching, so the scan budget is spent on the runs the client
    // asked to see first.
    sort_runs(&mut runs, &resolved);

    let (runs, scan_truncated) = apply_search(runs, &resolved);
    // Null when the scan was cut short: a count taken from a partial scan is
    // worse than no count, because a UI renders it as fact.
    let total = (!scan_truncated).then_some(runs.len());

    let (page_runs, next_cursor) = paginate(runs, &resolved);
    let items = page_runs
        .iter()
        .map(|meta| {
            let highlights = resolved
                .q
                .as_deref()
                .map(|q| highlights_for(meta, q, &resolved.sources))
                .unwrap_or_default();
            build_item(meta, &resolved, Some(highlights))
        })
        .collect();

    let mut page = Page::new(items, next_cursor, total, server_time);
    page.scan_truncated = scan_truncated;
    Ok(Json(page))
}

/// Order by `(sort value, run_id)`, with the tie-break following the primary
/// direction.
///
/// The tie-break is not decoration: two runs can start in the same second, and
/// a keyset walk over a non-total order drops whichever colliding run it
/// resumed past. Run ids are unique, so this makes the order total.
fn sort_runs(runs: &mut [RunMeta], resolved: &Resolved) {
    runs.sort_by(|a, b| {
        let ka = (resolved.sort.value(a), a.run_id.as_str());
        let kb = (resolved.sort.value(b), b.run_id.as_str());
        if resolved.descending {
            kb.cmp(&ka)
        } else {
            ka.cmp(&kb)
        }
    });
}

/// Phase one of search: keep the runs that could match, bounding how many of
/// them are allowed to cost a file read.
fn apply_search(runs: Vec<RunMeta>, resolved: &Resolved) -> (Vec<RunMeta>, bool) {
    let Some(ref q) = resolved.q else {
        return (runs, false);
    };
    let budgeted = resolved.searches_filesystem();
    let mut kept = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    for meta in runs {
        if budgeted {
            if scanned >= MAX_SEARCH_SCAN {
                truncated = true;
                break;
            }
            scanned += 1;
        }
        if matches_query(&meta, q, &resolved.sources) {
            kept.push(meta);
        }
    }
    (kept, truncated)
}

/// Does this run match, according to the requested sources? Sources are OR-ed.
///
/// Nothing here parses. The cheap sources read already-parsed metadata; the
/// deep ones substring-scan raw file bytes. Parsing is phase two's job, and it
/// only happens for the items actually being returned.
fn matches_query(meta: &RunMeta, q: &str, sources: &[Source]) -> bool {
    sources.iter().any(|source| match source {
        Source::Meta => meta_fields(meta)
            .iter()
            .any(|(_, text)| search::find_ignore_ascii_case(text, q).is_some()),
        Source::Files => meta
            .flags
            .modified_files
            .iter()
            .any(|path| search::find_ignore_ascii_case(path, q).is_some()),
        Source::Context => scan_file(&runstate::run_dir(&meta.run_id).join("context.json"), q),
        Source::Journal => scan_file(&runstate::run_dir(&meta.run_id).join("run.lvr"), q),
        Source::Logs => stage_indices(&meta.run_id).iter().any(|idx| {
            let output = runstate::tail_stage_output(&meta.run_id, *idx, SEARCH_LOG_TAIL_BYTES);
            let operational = runstate::tail_stage_log(&meta.run_id, *idx, SEARCH_LOG_TAIL_BYTES);
            search::find_ignore_ascii_case(&output, q).is_some()
                || search::find_ignore_ascii_case(&operational, q).is_some()
        }),
    })
}

/// The stage indices a run recorded, from `stages.json` - the index of record,
/// rather than a `read_dir` of the directory its bytes happened to land in.
fn stage_indices(run_id: &str) -> Vec<usize> {
    runstate::read_stages_index(run_id)
        .iter()
        .map(|stage| stage.index)
        .collect()
}

/// Substring-scan a whole file's bytes without parsing it.
fn scan_file(path: &std::path::Path, q: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => search::contains_ignore_ascii_case(&bytes, q.as_bytes()).is_some(),
        Err(_) => false,
    }
}

/// The searchable `(name, text)` pairs already present in a `RunMeta`.
fn meta_fields(meta: &RunMeta) -> Vec<(String, String)> {
    let mut out = vec![
        ("run_id".to_string(), meta.run_id.clone()),
        ("agent_name".to_string(), meta.agent_name.clone()),
        ("agent_path".to_string(), meta.agent_path.clone()),
        ("task".to_string(), meta.task.clone()),
        ("workdir".to_string(), meta.workdir.clone()),
        ("current_stage".to_string(), meta.current_stage.clone()),
    ];
    if let Some(ref title) = meta.title {
        out.push(("title".to_string(), title.clone()));
    }
    if let Some(ref model) = meta.model {
        out.push(("model".to_string(), model.clone()));
    }
    if let Some(ref error) = meta.error {
        out.push(("error".to_string(), error.clone()));
    }
    // `callback_url` and `callback_secret` are deliberately absent. The secret
    // never leaves the process, and neither is something a user searches for.
    // Sorted so the highlight a search reports for a metadata match does not
    // depend on hash order.
    let mut entries: Vec<(&String, &String)> = meta.metadata.iter().collect();
    entries.sort();
    for (key, value) in entries {
        out.push((format!("metadata.{key}"), value.clone()));
    }
    out
}

/// Phase two: why this run matched, for the items actually being returned.
fn highlights_for(meta: &RunMeta, q: &str, sources: &[Source]) -> Vec<Highlight> {
    let mut out = Vec::new();
    for source in sources {
        if out.len() >= MAX_HIGHLIGHTS {
            break;
        }
        match source {
            Source::Meta => {
                for (field, text) in meta_fields(meta) {
                    if out.len() >= MAX_HIGHLIGHTS {
                        break;
                    }
                    if let Some(at) = search::find_ignore_ascii_case(&text, q) {
                        out.push(Highlight {
                            field,
                            snippet: search::snippet(&text, at),
                            stage: None,
                        });
                    }
                }
            }
            Source::Files => {
                if let Some(path) = meta
                    .flags
                    .modified_files
                    .iter()
                    .find(|p| search::find_ignore_ascii_case(p, q).is_some())
                {
                    out.push(Highlight {
                        field: "modified_files".to_string(),
                        snippet: path.clone(),
                        stage: None,
                    });
                }
            }
            Source::Context => out.extend(context_highlight(meta, q)),
            Source::Logs => out.extend(logs_highlights(meta, q)),
            Source::Journal => out.extend(journal_highlights(meta, q)),
        }
    }
    out.truncate(MAX_HIGHLIGHTS);
    out
}

/// Where in the run's context window the match is, named by region.
///
/// Parses `context.json` once. Never replays the journal: that deep-copies a
/// whole context window per recorded point, which is the cost this design
/// exists to avoid.
fn context_highlight(meta: &RunMeta, q: &str) -> Option<Highlight> {
    let snapshot = runstate::read_context_snapshot(&meta.run_id)?;
    snapshot.regions.iter().find_map(|region| {
        region.entries.iter().find_map(|entry| {
            search::find_ignore_ascii_case(&entry.content, q).map(|at| Highlight {
                field: format!("context.{}", region.name),
                snippet: search::snippet(&entry.content, at),
                stage: None,
            })
        })
    })
}

/// Which stage's log the match is in - so a client can then fetch that stage.
///
/// One highlight per stage at most, and the two streams are tried in the order
/// a person reads them: the assistant's own output first, the operational log
/// second. Expressed as a `find_map` rather than a loop with early returns
/// because the caller already caps the total, so there is nothing here that
/// needs to bail out partway.
fn logs_highlights(meta: &RunMeta, q: &str) -> Vec<Highlight> {
    stage_indices(&meta.run_id)
        .into_iter()
        .filter_map(|idx| {
            let output = runstate::tail_stage_output(&meta.run_id, idx, SEARCH_LOG_TAIL_BYTES);
            if let Some(at) = search::find_ignore_ascii_case(&output, q) {
                return Some(Highlight {
                    field: "logs.output".to_string(),
                    snippet: search::snippet(&output, at),
                    stage: Some(idx),
                });
            }
            let operational = runstate::tail_stage_log(&meta.run_id, idx, SEARCH_LOG_TAIL_BYTES);
            search::find_ignore_ascii_case(&operational, q).map(|at| Highlight {
                field: "logs.operational".to_string(),
                snippet: search::snippet(&operational, at),
                stage: Some(idx),
            })
        })
        .take(MAX_HIGHLIGHTS)
        .collect()
}

/// Where in the run's history the match is: a tool call, or the context as it
/// stood at some earlier point.
///
/// Both halves matter. Live-testing this against real journals turned up runs
/// that matched on `q_in=journal` and came back with **no highlight at all** -
/// a result with no explanation, which is precisely what search-on-the-server
/// was supposed to fix. The text was in the journal's context records, and only
/// tool batches were being looked at.
///
/// Reads entry *content* and tool calls, and deliberately never the `meta` field
/// of `Header`/`Progress`/`Checkpoint`. Those carry a whole `RunMeta` including
/// the webhook signing secret, and a snippet cut from those bytes would put it
/// in the response. That exclusion is structural - the code never reaches for
/// the field - rather than a filter applied afterwards.
///
/// One residual case is left, and documented rather than papered over: the phase
/// one filter scans the journal's raw bytes, which *do* include those repeated
/// metadata blocks. A query matching only there (a workdir path, say) yields a
/// run with no highlight. The same text is searchable, with a highlight, through
/// `q_in=meta`.
fn journal_highlights(meta: &RunMeta, q: &str) -> Option<Highlight> {
    use leviath_core::run_archive::{RegionDelta, RunRecord};

    /// The first entry in a region whose content matches, named by region.
    fn in_entries(
        region_name: &str,
        entries: &[leviath_core::run_meta::RegionEntrySnapshot],
        q: &str,
    ) -> Option<Highlight> {
        entries.iter().find_map(|entry| {
            search::find_ignore_ascii_case(&entry.content, q).map(|at| Highlight {
                field: format!("journal.context.{region_name}"),
                snippet: search::snippet(&entry.content, at),
                stage: None,
            })
        })
    }

    /// The first match in one record, or `None` if it carries no matching text.
    fn in_record(record: &RunRecord, q: &str) -> Option<Highlight> {
        match record {
            RunRecord::ToolBatch {
                calls, stage_index, ..
            } => calls.iter().find_map(|call| {
                [&call.arguments, call.result.as_ref().unwrap_or(&call.name)]
                    .into_iter()
                    .find_map(|text| {
                        search::find_ignore_ascii_case(text, q).map(|at| Highlight {
                            field: format!("journal.tool.{}", call.name),
                            snippet: search::snippet(text, at),
                            stage: Some(*stage_index),
                        })
                    })
            }),
            RunRecord::ContextCheckpoint { snapshot, .. } => snapshot
                .regions
                .iter()
                .find_map(|region| in_entries(&region.name, &region.entries, q)),
            RunRecord::ContextDiff { delta, .. } | RunRecord::Progress { delta, .. } => {
                delta.regions.iter().find_map(|region| match region {
                    RegionDelta::Set(snapshot) => in_entries(&snapshot.name, &snapshot.entries, q),
                    RegionDelta::Append { name, entries, .. } => in_entries(name, entries, q),
                    // Carry no text of their own.
                    RegionDelta::Clear { .. } | RegionDelta::Remove { .. } => None,
                })
            }
            RunRecord::Checkpoint { context, .. } => context
                .regions
                .iter()
                .find_map(|region| in_entries(&region.name, &region.entries, q)),
            // Carry no searchable content of their own - only the metadata this
            // function must not cut a snippet from.
            RunRecord::Header { .. }
            | RunRecord::OwnershipChanged { .. }
            | RunRecord::StatusChanged { .. }
            | RunRecord::Inference { .. }
            | RunRecord::InferenceUsage { .. }
            | RunRecord::ToolCallDone { .. }
            | RunRecord::Message { .. } => None,
        }
    }

    // Streamed, stopping at the first matching record: parsing the whole
    // journal per returned item multiplied the history endpoint's biggest
    // allocation by the page size.
    let mut found = None;
    runstate::visit_run_records(&meta.run_id, &mut |record| match in_record(record, q) {
        Some(hit) => {
            found = Some(hit);
            std::ops::ControlFlow::Break(())
        }
        None => std::ops::ControlFlow::Continue(()),
    })?;
    found
}

/// Take this page's runs and mint the cursor for the next one.
///
/// Takes `limit + 1` and keeps `limit`, so a cursor is only ever emitted when a
/// further item is known to exist. Emitting one speculatively would make a
/// client's "loop until null" run one empty request longer, every time.
fn paginate(runs: Vec<RunMeta>, resolved: &Resolved) -> (Vec<RunMeta>, Option<String>) {
    let mut after_cursor: Vec<RunMeta> = match resolved.cursor {
        None => runs,
        Some(ref cursor) => runs
            .into_iter()
            .filter(|meta| {
                cursor.precedes(
                    &CursorKey::Int(resolved.sort.value(meta)),
                    &meta.run_id,
                    resolved.descending,
                )
            })
            .collect(),
    };

    let has_more = after_cursor.len() > resolved.limit;
    after_cursor.truncate(resolved.limit);
    let next = has_more.then(|| after_cursor.last()).flatten().map(|last| {
        cursor::encode(
            resolved.sort.as_str(),
            if resolved.descending { "desc" } else { "asc" },
            &resolved.digest,
            CursorKey::Int(resolved.sort.value(last)),
            &last.run_id,
        )
    });
    (after_cursor, next)
}

/// One run as this server hands it out: redacted, and carrying the two spans a
/// caller would otherwise have to compute.
///
/// The single place a `RunMeta` becomes JSON on any route. `redacted()` is what
/// strips the webhook signing secret, and a redaction that has to be remembered
/// per handler is the one that gets forgotten; the same goes for the spans,
/// which are the reason `/api/runs` and `/api/agents` used to describe the same
/// run with different keys.
pub(super) fn run_json(meta: &RunMeta, now: i64) -> serde_json::Value {
    let mut value = serde_json::to_value(meta.redacted()).unwrap_or(serde_json::Value::Null);
    leviath_core::duration::annotate_spans(
        &mut value,
        meta.age_secs(now),
        meta.active_runtime_secs(now),
    );
    value
}

/// Build one response item, redacting and then projecting.
///
/// The spans go on before the projection, so `?fields=working_secs` selects one
/// the way it selects any other key.
fn build_item(meta: &RunMeta, resolved: &Resolved, highlights: Option<Vec<Highlight>>) -> RunItem {
    let mut value = run_json(meta, leviath_core::duration::now_secs());
    if let (Some(fields), serde_json::Value::Object(map)) = (&resolved.fields, &mut value) {
        map.retain(|key, _| fields.contains(key));
    }
    RunItem {
        meta: value,
        highlights: highlights.unwrap_or_default(),
    }
}

#[cfg(test)]
#[path = "runs_tests.rs"]
mod tests;

/// Why a run named in a bulk delete was left alone.
#[derive(Debug, Serialize)]
pub(super) struct SkippedRun {
    /// The run that was not deleted.
    pub(super) id: String,
    /// A sentence saying why, for a console to show verbatim.
    pub(super) reason: String,
}

/// What a bulk delete did.
///
/// Reports per-run outcomes rather than a count, because the interesting result
/// of "clear everything older than a month" is which runs survived it: a live
/// run and a run that was already gone are both non-deletions and a caller that
/// only got `deleted: 12` cannot tell them apart, or tell the user why the list
/// did not empty.
#[derive(Debug, Serialize)]
pub(super) struct DeleteRunsResp {
    /// Ids whose directories are gone.
    pub(super) deleted: Vec<String>,
    /// Ids that were left, each with a reason.
    pub(super) skipped: Vec<SkippedRun>,
}

/// Query for `DELETE /api/runs/{id}`.
#[derive(Debug, Deserialize)]
pub(super) struct DeleteRunQuery {
    /// Delete a run whose record cannot be read, which is otherwise a 409.
    pub(super) force: Option<bool>,
}

/// Query for `DELETE /api/runs`.
#[derive(Debug, Deserialize)]
pub(super) struct DeleteRunsQuery {
    /// Delete every finished run last updated strictly before this unix time.
    pub(super) before: Option<i64>,
    /// Delete exactly these runs, comma-separated.
    pub(super) ids: Option<String>,
}

/// Whether a run may be removed, or the reason it may not.
///
/// One definition for the single and bulk routes, so a run that 409s on its own
/// cannot be silently deleted as part of a sweep.
///
/// `force` covers only the last case below, and only the single-run route ever
/// passes it.
fn deletable(id: &str, force: bool) -> Result<(), (StatusCode, String)> {
    let dir = runstate::run_dir(id);
    if !dir.exists() {
        return Err((StatusCode::NOT_FOUND, format!("Run '{id}' not found")));
    }
    // Judged from the run's own record, not by asking the daemon: a daemon that
    // is down must not make every run undeletable.
    match runstate::read_meta(id) {
        Ok(meta) if runstate::is_terminal_status(&meta.status) => Ok(()),
        Ok(meta) => Err((
            StatusCode::CONFLICT,
            format!(
                "Run '{id}' is {}; cancel it before deleting it",
                meta.status
            ),
        )),
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
        Err(e) => Err((
            StatusCode::CONFLICT,
            format!(
                "Run '{id}' has no readable record ({e}), so it cannot be shown \
                 to be finished; pass force=true to delete it anyway"
            ),
        )),
    }
}

/// Remove a run's directory, having already decided it may go.
fn remove_run(id: &str) -> Result<(), (StatusCode, String)> {
    std::fs::remove_dir_all(runstate::run_dir(id)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete run '{id}': {e}"),
        )
    })
}

/// Whether every member of `ids` may go, or the reason one of them may not.
///
/// A live sub-agent blocks the whole delete rather than being skipped: half a
/// tree is not a state anything downstream knows how to read, and removing the
/// parent of a running agent is exactly what [`deletable`] refuses to do for
/// the run named directly. The reason names the sub-agent, because "cancel it
/// before deleting it" about a run the caller never mentioned is unactionable.
fn deletable_family(root: &str, ids: &[String], force: bool) -> Result<(), (StatusCode, String)> {
    for id in ids {
        deletable(id, force).map_err(|(code, msg)| {
            if id == root {
                (code, msg)
            } else {
                (
                    code,
                    format!("{msg}. It is a sub-agent run of '{root}', deleted with it"),
                )
            }
        })?;
    }
    Ok(())
}

/// Remove every run in `ids`, stopping at the first failure.
fn remove_family(ids: &[String]) -> Result<(), (StatusCode, String)> {
    for id in ids {
        remove_run(id)?;
    }
    Ok(())
}

/// `DELETE /api/runs/{id}`: remove a finished run's record from disk.
///
/// Separate from `DELETE /api/agents/{id}`, which cancels. The two verbs mean
/// genuinely different things - one stops the work, the other forgets it
/// happened - and answering 204 to both would leave a client unable to say
/// which it got (issue #463).
///
/// The deletion is real: the directory and everything in it, including the
/// transcript. That is the point of the route. A console that offered a
/// "Delete" which only hid the run locally would tell somebody clearing a
/// sensitive transcript that it was gone when it was not.
///
/// It is also the whole sub-agent tree - see [`runstate::family_of`]. Deleting a parent and
/// leaving its children behind left them on disk with nothing above them, and
/// a client that nests runs under their parent (the dashboard does) has
/// nowhere to draw them but the top level, so a delete read as a promotion.
/// Deleting a child never touches its parent or its siblings.
///
/// **409** on a live run - removing a directory out from under a running agent
/// is a different and much larger feature, and refusing is the honest answer.
/// **404** on a run that is already gone, so a client that lost the response to
/// its own delete can repeat it rather than treat a missing run as a failure.
/// **409** too on a run whose record will not parse, which `force=true`
/// overrides; see [`deletable`] for why that one is not automatic.
pub(super) async fn delete_run(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DeleteRunQuery>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    // `run_dir` maps an unsafe id to a path that cannot exist, so a traversal
    // attempt arrives here as an ordinary miss rather than a removed directory.
    let ids = runstate::family_of(&id);
    deletable_family(&id, &ids, query.force.unwrap_or(false))
        .map_err(|(code, msg)| err(code, msg))?;
    remove_family(&ids).map_err(|(code, msg)| err(code, msg))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/runs?before=<unix>` or `?ids=a,b`: prune many runs at once.
///
/// The realistic use is "clear everything older than a month", and one request
/// per run over a few hundred runs is its own problem.
///
/// Partial success is the normal outcome, not an error: a sweep that meets one
/// live run has still correctly deleted the rest, so this answers 200 with the
/// per-run verdicts rather than failing the whole request. The single-run route
/// is the one that reports a status per outcome.
///
/// Every named run takes its sub-agent tree with it, as on the single-run
/// route, so `deleted` can hold ids the caller never mentioned. Reporting them
/// is the point: they are the runs that are now gone.
///
/// Neither parameter is a **400** rather than "every run": a bulk delete with no
/// predicate is much more likely to be a client that failed to build its query
/// than an operator asking to erase the machine's entire history.
pub(super) async fn delete_runs(
    Query(query): Query<DeleteRunsQuery>,
) -> Result<Json<DeleteRunsResp>, (StatusCode, Json<ErrorResponse>)> {
    let targets: Vec<String> = match (&query.ids, query.before) {
        (Some(ids), _) => {
            let ids = comma_list(ids);
            if ids.len() > MAX_IDS {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "`ids` names {} runs; at most {MAX_IDS} may be deleted at once",
                        ids.len()
                    ),
                ));
            }
            ids
        }
        // Scoped to terminal runs at selection time as well as in `deletable`,
        // so a sweep does not report every live run on the machine as skipped.
        (None, Some(before)) => runstate::list_runs()
            .into_iter()
            .filter(|m| runstate::is_terminal_status(&m.status) && m.updated_at < before)
            .map(|m| m.run_id)
            .collect(),
        (None, None) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "a bulk delete needs `before` or `ids`; refusing to delete every run".to_string(),
            ));
        }
    };

    let mut deleted: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    for id in targets {
        // A sweep by `before` selects a parent and its children independently,
        // and naming a parent already took its children; either way the second
        // mention is of a run this request has just removed, which is a
        // deletion rather than the 404 `deletable` would report.
        if deleted.contains(&id) {
            continue;
        }
        let ids = runstate::family_of(&id);
        // Never forced. A sweep names runs by a predicate rather than one at a
        // time, so an unreadable record inside it is far likelier to be
        // collateral than the thing the operator meant to clear.
        match deletable_family(&id, &ids, false).and_then(|()| remove_family(&ids)) {
            Ok(()) => deleted.extend(ids),
            Err((_, reason)) => skipped.push(SkippedRun { id, reason }),
        }
    }
    Ok(Json(DeleteRunsResp { deleted, skipped }))
}
