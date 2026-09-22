//! `GET /api/runs` - the paginated, searchable run listing.
//!
//! Supersedes `GET /api/agents`, which returns every run ever recorded as one
//! unbounded array and accepts only a status filter. That route stays exactly as
//! it is, deprecated: it is the older spelling (the console says "runs"
//! everywhere), and it gets a replacement at a new path rather than a changed
//! response shape, so nothing that calls it today breaks.
//!
//! What this adds over that: keyset pagination, sorting, server-side search with
//! highlights, batch fetch by id, and field projection.
//!
//! Every listing here starts from the shared run index (`run_index`), which
//! parses a `meta.json` only when its stat changes, so a page of fifty costs a
//! stat per live run rather than a parse of every run on the machine.
//! Pagination bounds what crosses the wire and what the browser holds. The
//! guard that bounds the filesystem-reading half of search is
//! [`MAX_SEARCH_SCAN`].
//!
//! Pruning is [`delete_run`] and [`delete_runs`], which is the other half of
//! that story: the listing can now be made smaller, not only paged over.

use std::collections::HashSet;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::core::error::{ServeError, as_api_error};
use super::core::runs::{
    self as run_core, MAX_IDS, MAX_LIMIT, ParentFilter, RunSpec, SortKey, Source,
};
use super::types::*;
use crate::runstate::{self, RunMeta};

/// Page size when the client does not ask.
const DEFAULT_LIMIT: usize = 50;

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
    /// `descendant_of=<run_id>`: that run's whole subtree, at any depth, and not
    /// the run itself. The flat read of a fan-out, where `parent=` is one level.
    pub(super) descendant_of: Option<String>,
    /// `blueprint=<name>`: only runs of that blueprint, by recorded name.
    pub(super) blueprint: Option<String>,
}

/// A request this route refuses to answer, as the shared failure type.
///
/// Both surfaces render it: REST as a 400 with the message in the body,
/// GraphQL as an `errors` entry coded `BAD_USER_INPUT`.
fn bad_request(message: String) -> ServeError {
    ServeError::BadRequest(message)
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

fn resolve(query: &RunsQuery) -> Result<RunSpec, ServeError> {
    // `ids` is a batch fetch, not a filter: it names exactly what it wants, so
    // paging, ordering and filtering have nothing to act on. Rejecting the
    // combination is deliberate - a silently ignored parameter produces the
    // kind of bug report that takes a day to read.
    // One question about parentage per listing. `parent=` and `descendant_of=`
    // ask two different ones, and the pair a caller meant is not recoverable
    // from the pair they sent.
    let parent = match (query.parent.as_deref(), query.descendant_of.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(bad_request(
                "`parent` names one run's children and `descendant_of` names a whole subtree, \
                 so only one of them may be set"
                    .to_string(),
            ));
        }
        (_, Some(root)) => ParentFilter::Under(root.to_string()),
        (parent, None) => ParentFilter::parse(parent),
    };
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
            ("blueprint", query.blueprint.is_some()),
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
            let known = known_fields();
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

    run_core::RunSelection {
        limit,
        statuses,
        sort,
        descending,
        // One key orders this route, which `sort` and `descending` already
        // say; several is the other surface's.
        order: None,
        q,
        sources,
        sources_raw: sources_raw.to_string(),
        fields,
        ids,
        since: query.since,
        parent,
        blueprint: query.blueprint.clone(),
        // The flat query parameters above are the whole filter this route
        // takes; a composable predicate is the other surface's.
        predicate: None,
        // This route holds no records of its own: a batch fetch by id reads
        // each one it names.
        preloaded: None,
    }
    .resolve(query.cursor.as_deref())
}

/// The top-level keys of a serialized `RunMeta`, for validating `fields`.
///
/// Derived from an actual serialization rather than a hand-written list, so the
/// allowlist cannot drift away from the struct when a field is added.
///
/// Every `Option` field is filled first. Several carry
/// `skip_serializing_if = "Option::is_none"`, so a probe left at its defaults
/// omits them and the allowlist silently refuses a field that does exist -
/// `?fields=waiting_on` on a parked run, say, which is what the leviath.dev
/// console asks for on every sidebar load. Filling the options is what makes
/// the sentence above true, and
/// `every_skip_if_none_option_on_run_meta_is_filled_by_the_probe` in the tests
/// reads the struct's source to catch the next one added without a line here.
pub(super) fn known_fields() -> HashSet<String> {
    serialized_keys(&probe_meta())
}

/// A `RunMeta` with every `skip_serializing_if` field filled, so that
/// serializing it names every key a real run can carry.
fn probe_meta() -> RunMeta {
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
    probe.waiting_on = Some(leviath_core::run_meta::WaitReason::ToolApproval);
    probe.output_request = Some(Default::default());
    probe.model_override = Some(String::new());
    probe.yolo_profile = Some(String::new());
    probe.blueprint_digest = Some(String::new());
    probe.stage_models = vec![leviath_core::run_meta::StageModelUse {
        provider: String::new(),
        model: String::new(),
    }];
    probe
}

/// The top-level keys `probe` serializes to, plus the two the route adds.
fn serialized_keys(probe: &RunMeta) -> HashSet<String> {
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
///
/// Parses the query, hands the listing to the service layer, and renders the
/// page. Filtering, ordering, searching and paging all live in
/// [`run_core::list`], which the GraphQL `runs` field calls with a spec built
/// from its own arguments.
pub(super) async fn list_runs(
    State(state): State<AppState>,
    Query(query): Query<RunsQuery>,
) -> Result<Json<Page<RunItem>>, ApiError> {
    let spec = resolve(&query).map_err(|e| as_api_error(&e))?;
    let listing = run_core::list(&state, &spec).await;
    let items = listing
        .hits
        .iter()
        .map(|hit| build_item(&hit.meta, &spec, Some(hit.highlights.clone())))
        .collect();
    let mut page = Page::new(
        items,
        listing.next_cursor,
        listing.total,
        listing.server_time,
    );
    page.scan_truncated = listing.scan_truncated;
    page.missing = listing.missing;
    Ok(Json(page))
}

/// One run as this server hands it out: redacted, and carrying the two spans a
/// caller would otherwise have to compute.
///
/// The single place a `RunMeta` becomes JSON on any route. `redacted()` is what
/// strips the webhook signing secret, and a redaction that has to be remembered
/// per handler is the one that gets forgotten; the same goes for the spans,
/// which are what keeps `/api/runs` and `/api/agents` describing the same run
/// with the same keys.
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
fn build_item(meta: &RunMeta, resolved: &RunSpec, highlights: Option<Vec<Highlight>>) -> RunItem {
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

/// `DELETE /api/runs/{id}`: remove a finished run's record from disk.
///
/// Separate from `DELETE /api/agents/{id}`, which cancels. The two verbs mean
/// genuinely different things - one stops the work, the other forgets it
/// happened - and answering 204 to both would leave a client unable to say
/// which it got.
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
) -> Result<StatusCode, ApiError> {
    // `run_dir` maps an unsafe id to a path that cannot exist, so a traversal
    // attempt arrives here as an ordinary miss rather than a removed directory.
    let ids = runstate::family_of(&id);
    run_core::deletable_family(&id, &ids, query.force.unwrap_or(false))
        .map_err(|e| as_api_error(&e))?;
    run_core::remove_family(&ids).map_err(|e| as_api_error(&e))?;
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
    State(state): State<AppState>,
    Query(query): Query<DeleteRunsQuery>,
) -> Result<Json<DeleteRunsResp>, ApiError> {
    let targets = match (&query.ids, query.before) {
        (Some(ids), _) => run_core::DeleteTargets::Ids(comma_list(ids)),
        (None, Some(before)) => run_core::DeleteTargets::Before(before),
        (None, None) => {
            return Err(as_api_error(&ServeError::BadRequest(
                "a bulk delete needs `before` or `ids`; refusing to delete every run".to_string(),
            )));
        }
    };
    // Never forced. A sweep names runs by a predicate rather than one at a
    // time, so an unreadable record inside it is far likelier to be collateral
    // than the thing the operator meant to clear.
    let outcome = run_core::delete(&state, targets, false)
        .await
        .map_err(|e| as_api_error(&e))?;
    Ok(Json(DeleteRunsResp {
        deleted: outcome.deleted,
        skipped: outcome
            .skipped
            .into_iter()
            .map(|skipped| SkippedRun {
                id: skipped.id,
                reason: skipped.reason,
            })
            .collect(),
    }))
}
