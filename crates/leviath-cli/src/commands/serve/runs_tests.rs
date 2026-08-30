//! Tests for the paginated, searchable run listing.
//!
//! The pure halves - query resolution, ordering, paging, projection - are
//! exercised directly over in-memory `RunMeta` values, so every 400 and every
//! ordering rule is a plain unit test with no HTTP and no temp directory. Only
//! the tests that genuinely need files on disk take the isolated-runs-dir path.

use super::*;
use crate::runstate::{RunStatus, create_run};

// ─── fixtures ───────────────────────────────────────────────────────────────

fn meta_at(id: &str, started_at: i64) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        "test-agent".to_string(),
        "/agents/test".to_string(),
        "do the thing".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.started_at = started_at;
    meta.updated_at = started_at;
    meta
}

/// A run started by `parent`, which is what makes it a sub-agent rather than a
/// row a top-level listing draws.
fn child_of(id: &str, started_at: i64, parent: &str) -> RunMeta {
    let mut meta = meta_at(id, started_at);
    meta.parent_run_id = Some(parent.to_string());
    meta
}

/// Build a `RunsQuery` the way a real request would, through axum's own
/// extractor, so these tests cannot pass on a struct shape the wire never
/// produces.
fn query(pairs: &[(&str, &str)]) -> RunsQuery {
    let encoded = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let uri: axum::http::Uri = format!("http://test/api/runs?{encoded}")
        .parse()
        .expect("uri parses");
    Query::<RunsQuery>::try_from_uri(&uri)
        .expect("query parses")
        .0
}

fn urlencode(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | ',' => c.to_string(),
            other => other
                .to_string()
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect(),
        })
        .collect()
}

fn resolve_ok(pairs: &[(&str, &str)]) -> Resolved {
    match resolve(&query(pairs)) {
        Ok(resolved) => resolved,
        Err((_, body)) => panic!("expected resolve to succeed: {}", body.0.error),
    }
}

fn resolve_err(pairs: &[(&str, &str)]) -> String {
    match resolve(&query(pairs)) {
        Ok(_) => String::new(),
        Err((status, body)) => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            body.0.error.clone()
        }
    }
}

fn ids(runs: &[RunMeta]) -> Vec<&str> {
    runs.iter().map(|m| m.run_id.as_str()).collect()
}

// ─── query resolution ───────────────────────────────────────────────────────

#[test]
fn defaults_are_a_descending_started_at_page_of_fifty_over_the_cheap_sources() {
    let r = resolve_ok(&[]);
    assert_eq!(r.limit, DEFAULT_LIMIT);
    assert_eq!(r.sort, SortKey::Started);
    assert!(r.descending);
    assert!(r.q.is_none());
    assert_eq!(r.sources, vec![Source::Meta, Source::Files]);
    assert!(r.fields.is_none());
    assert!(!r.searches_filesystem());
}

/// A client asking for more than the cap wants as much as it can get, so the
/// value is clamped rather than the request refused.
#[test]
fn an_oversized_limit_is_clamped_and_a_zero_limit_is_refused() {
    assert_eq!(resolve_ok(&[("limit", "100000")]).limit, MAX_LIMIT);
    assert_eq!(resolve_ok(&[("limit", "5")]).limit, 5);
    assert!(resolve_err(&[("limit", "0")]).contains("at least 1"));
}

#[test]
fn an_unknown_sort_order_or_source_is_refused_by_name() {
    assert!(resolve_err(&[("sort", "whenever")]).contains("whenever"));
    assert!(resolve_err(&[("order", "sideways")]).contains("sideways"));
    assert!(resolve_err(&[("q_in", "everything")]).contains("everything"));
}

#[test]
fn every_sort_key_and_order_resolves() {
    assert_eq!(resolve_ok(&[("sort", "updated_at")]).sort, SortKey::Updated);
    assert_eq!(
        resolve_ok(&[("sort", "last_progress_at")]).sort,
        SortKey::LastProgress
    );
    assert!(!resolve_ok(&[("order", "asc")]).descending);
}

#[test]
fn duplicate_and_empty_search_sources_are_tolerated() {
    let r = resolve_ok(&[("q_in", "meta,,meta,files,")]);
    assert_eq!(r.sources, vec![Source::Meta, Source::Files]);
}

#[test]
fn a_filesystem_source_is_only_budgeted_when_there_is_a_query() {
    assert!(!resolve_ok(&[("q_in", "logs")]).searches_filesystem());
    assert!(resolve_ok(&[("q", "x"), ("q_in", "logs")]).searches_filesystem());
    assert!(!resolve_ok(&[("q", "x"), ("q_in", "meta")]).searches_filesystem());
}

/// An identity-less item is useless to every client, so `run_id` is not
/// something a projection can drop.
#[test]
fn fields_always_keeps_run_id_and_rejects_what_it_cannot_serve() {
    let r = resolve_ok(&[("fields", "status,title")]);
    let fields = r.fields.expect("fields set");
    assert!(fields.contains("run_id"));
    assert!(fields.contains("status"));

    assert!(resolve_err(&[("fields", "nonsense")]).contains("nonsense"));
    // The nested case gets its own message: "unknown field" would be a
    // misleading answer to a reasonable thing to try.
    assert!(resolve_err(&[("fields", "flags.modified_file_count")]).contains("top-level"));
}

/// A field that only appears on runs that have it is still a field.
///
/// `known_meta_fields` builds its allowlist by serializing a probe `RunMeta`,
/// and several fields carry `skip_serializing_if = "Option::is_none"`. A probe
/// left at its defaults omits those, so asking for one was refused as unknown
/// even on a run that carried it. The probe fills every option to keep the
/// allowlist honest.
#[test]
fn fields_accepts_the_optional_ones_that_only_some_runs_carry() {
    for optional in [
        "read_paths",
        "final_output",
        "waiting_on",
        "output_request",
        "model_override",
    ] {
        let r = resolve_ok(&[("fields", optional)]);
        let fields = r.fields.expect("fields set");
        assert!(fields.contains(optional), "{optional} should be selectable");
    }
}

/// The probe has to name every field the struct has, and the only place that
/// list exists is the struct itself. So read it: every `pub` field declared
/// inside `RunMeta` in `run_meta.rs` must be a key the probe serializes. A
/// field added with `skip_serializing_if` and no line in `probe_meta` fails
/// here rather than as a 400 in a console.
#[test]
fn every_skip_if_none_option_on_run_meta_is_filled_by_the_probe() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../leviath-core/src/run_meta.rs"
    ))
    .expect("the RunMeta source");
    // The struct's lines: from its declaration to the first column-zero `}`.
    let declared: Vec<&str> = source
        .lines()
        .skip_while(|line| *line != "pub struct RunMeta {")
        .skip(1)
        .take_while(|line| *line != "}")
        .filter_map(|line| line.trim().strip_prefix("pub "))
        .filter_map(|rest| rest.split_once(':'))
        .map(|(name, _)| name.trim())
        .collect();
    assert!(declared.len() > 30, "found only {declared:?}");

    let known = known_meta_fields();
    let missing: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|name| !known.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "RunMeta fields the probe does not serialize (fill them in probe_meta): {missing:?}"
    );
}

/// A silently ignored parameter is the API smell that produces the worst bug
/// reports, so each conflicting combination is refused by name.
#[test]
fn ids_cannot_be_combined_with_the_parameters_it_would_override() {
    for conflicting in ["cursor", "q", "status", "since", "parent"] {
        let message = resolve_err(&[("ids", "a,b"), (conflicting, "1")]);
        assert!(
            message.contains(conflicting),
            "expected {conflicting} to be named, got {message}"
        );
    }
    assert_eq!(
        resolve_ok(&[("ids", "a,b")]).ids,
        Some(vec!["a".to_string(), "b".to_string()])
    );
}

#[test]
fn too_many_ids_are_refused() {
    let many = (0..MAX_IDS + 1)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert!(resolve_err(&[("ids", &many)]).contains("at most"));
}

#[test]
fn an_empty_query_string_is_treated_as_no_search() {
    assert!(resolve_ok(&[("q", "")]).q.is_none());
}

/// The three things `parent` can mean, and the one spelling that is a keyword.
#[test]
fn parent_resolves_to_the_three_shapes_it_has() {
    assert_eq!(resolve_ok(&[]).parent, ParentFilter::Any);
    // Whitespace is the query-string equivalent of an empty box.
    assert_eq!(resolve_ok(&[("parent", "  ")]).parent, ParentFilter::Any);
    assert_eq!(
        resolve_ok(&[("parent", "none")]).parent,
        ParentFilter::Roots
    );
    assert_eq!(
        resolve_ok(&[("parent", "run-7")]).parent,
        ParentFilter::Of("run-7".to_string())
    );

    // And what each keeps, which is the half the handler leans on.
    let root = meta_at("root", 1);
    let child = child_of("child", 2, "root");
    assert!(ParentFilter::Any.keeps(&root) && ParentFilter::Any.keeps(&child));
    assert!(ParentFilter::Roots.keeps(&root) && !ParentFilter::Roots.keeps(&child));
    let of_root = ParentFilter::Of("root".to_string());
    assert!(of_root.keeps(&child) && !of_root.keeps(&root));
}

/// A cursor carries the filters it was minted under, so a walk cannot change
/// what it is filtering halfway through - which for this one would mean a
/// client paging past sub-agents it had asked not to see.
#[test]
fn a_cursor_is_bound_to_the_parent_filter_it_was_minted_for() {
    let roots = resolve_ok(&[("parent", "none")]);
    let raw = cursor::encode(
        roots.sort.as_str(),
        "desc",
        &roots.digest,
        CursorKey::Int(10),
        "run-a",
    );

    assert!(resolve(&query(&[("parent", "none"), ("cursor", &raw)])).is_ok());
    assert!(resolve_err(&[("cursor", &raw)]).contains("filters"));
    assert!(resolve_err(&[("parent", "run-7"), ("cursor", &raw)]).contains("filters"));

    // The unfiltered digest is unchanged by this parameter existing, so every
    // cursor a client is holding from before it stays valid.
    let anything = resolve_ok(&[]);
    let before = cursor::filter_digest(&["", "", "meta,files", ""]);
    assert_eq!(anything.digest, before);
}

// ─── cursor binding ─────────────────────────────────────────────────────────

/// A cursor names a position in a particular list. Presented against a
/// different sort, order or filter set it cannot mean anything, so it is a 400
/// rather than a page of quietly wrong results.
#[test]
fn a_cursor_is_bound_to_the_walk_it_was_minted_for() {
    let base = resolve_ok(&[("status", "running")]);
    let raw = cursor::encode(
        base.sort.as_str(),
        "desc",
        &base.digest,
        CursorKey::Int(10),
        "run-a",
    );

    assert!(resolve(&query(&[("status", "running"), ("cursor", &raw)])).is_ok());

    assert!(resolve_err(&[("status", "error"), ("cursor", &raw)]).contains("filters"));
    assert!(
        resolve_err(&[
            ("status", "running"),
            ("cursor", &raw),
            ("sort", "updated_at")
        ])
        .contains("sort=")
    );
    assert!(
        resolve_err(&[("status", "running"), ("cursor", &raw), ("order", "asc")])
            .contains("order=")
    );
}

#[test]
fn a_malformed_cursor_is_refused() {
    assert!(!resolve_err(&[("cursor", "zzzz")]).is_empty());
}

// ─── ordering and paging ────────────────────────────────────────────────────

#[test]
fn runs_sort_by_the_chosen_key_in_the_chosen_direction() {
    let mut runs = vec![meta_at("b", 200), meta_at("a", 100), meta_at("c", 300)];
    sort_runs(&mut runs, &resolve_ok(&[]));
    assert_eq!(ids(&runs), vec!["c", "b", "a"]);

    sort_runs(&mut runs, &resolve_ok(&[("order", "asc")]));
    assert_eq!(ids(&runs), vec!["a", "b", "c"]);
}

/// Two runs starting in the same second is ordinary, not exotic. Without the
/// id tie-break the order is not total, and a keyset walk drops whichever
/// colliding run it happened to resume past.
#[test]
fn runs_sharing_a_sort_value_are_broken_apart_by_id() {
    let mut runs = vec![meta_at("b", 100), meta_at("c", 100), meta_at("a", 100)];
    sort_runs(&mut runs, &resolve_ok(&[]));
    assert_eq!(ids(&runs), vec!["c", "b", "a"], "descending id tie-break");

    sort_runs(&mut runs, &resolve_ok(&[("order", "asc")]));
    assert_eq!(ids(&runs), vec!["a", "b", "c"], "ascending id tie-break");
}

/// Absent `last_progress_at` means "older daemon, or before the first
/// snapshot" - the run did start, so `started_at` is the honest floor, and it
/// keeps the sort key non-null for the cursor.
#[test]
fn a_missing_last_progress_at_falls_back_to_started_at() {
    let mut meta = meta_at("a", 500);
    meta.last_progress_at = None;
    assert_eq!(SortKey::LastProgress.value(&meta), 500);
    meta.last_progress_at = Some(900);
    assert_eq!(SortKey::LastProgress.value(&meta), 900);
    assert_eq!(SortKey::Updated.value(&meta), 500);
}

/// Walking the whole list a page at a time must visit every run exactly once.
/// That is the property keyset paging exists for, so it is asserted as a
/// property rather than on one hand-picked page.
#[test]
fn paging_all_the_way_through_visits_every_run_exactly_once() {
    let all: Vec<RunMeta> = (0..25)
        .map(|i| meta_at(&format!("run-{i:02}"), i))
        .collect();

    let mut seen: Vec<String> = Vec::new();
    let mut cursor_raw: Option<String> = None;
    for _ in 0..20 {
        let resolved = match cursor_raw {
            None => resolve_ok(&[("limit", "4")]),
            Some(ref c) => resolve_ok(&[("limit", "4"), ("cursor", c)]),
        };

        let mut runs = all.clone();
        sort_runs(&mut runs, &resolved);
        let (page, next) = paginate(runs, &resolved);

        assert!(page.len() <= 4);
        seen.extend(page.iter().map(|m| m.run_id.clone()));
        match next {
            Some(c) => cursor_raw = Some(c),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "a run was returned twice");
    assert_eq!(seen.len(), all.len(), "a run was skipped");
}

/// Emitting a cursor speculatively would make every client's "loop until null"
/// run one extra empty request, every single time.
#[test]
fn no_cursor_is_emitted_on_the_last_page() {
    let all: Vec<RunMeta> = (0..3).map(|i| meta_at(&format!("r{i}"), i)).collect();
    let (page, next) = paginate(all, &resolve_ok(&[("limit", "3")]));
    assert_eq!(page.len(), 3);
    assert!(next.is_none(), "exactly-full page must not promise more");
}

#[test]
fn an_empty_list_pages_to_nothing() {
    let (page, next) = paginate(Vec::new(), &resolve_ok(&[]));
    assert!(page.is_empty());
    assert!(next.is_none());
}

/// Keyset paging's whole point: an insert elsewhere in the list must not shift
/// the window, the way an offset would.
#[test]
fn a_run_arriving_at_the_head_does_not_shift_the_next_page() {
    let all: Vec<RunMeta> = (0..6)
        .map(|i| meta_at(&format!("run-{i}"), i * 10))
        .collect();
    let resolved = resolve_ok(&[("limit", "2")]);
    let mut sorted = all.clone();
    sort_runs(&mut sorted, &resolved);
    let (first_page, next) = paginate(sorted, &resolved);
    assert_eq!(ids(&first_page), vec!["run-5", "run-4"]);

    // A brand new run arrives at the head between the two requests.
    let mut with_new = all.clone();
    with_new.push(meta_at("run-9", 999));
    let raw = next.expect("more pages");
    let resolved2 = resolve_ok(&[("limit", "2"), ("cursor", &raw)]);
    let mut sorted2 = with_new;
    sort_runs(&mut sorted2, &resolved2);
    let (second_page, _) = paginate(sorted2, &resolved2);

    // The new run sorts before the cursor so the walk skips it, and crucially
    // nothing either side of it is dropped or repeated.
    assert_eq!(ids(&second_page), vec!["run-3", "run-2"]);
}

// ─── projection ─────────────────────────────────────────────────────────────

#[test]
fn without_fields_an_item_carries_the_whole_redacted_meta() {
    let item = build_item(&meta_at("a", 1), &resolve_ok(&[]), None);
    let map = item.meta.as_object().expect("object");
    assert!(map.contains_key("run_id"));
    assert!(map.contains_key("task"));
    assert!(map.contains_key("status"));
}

#[test]
fn fields_narrows_the_item_but_never_drops_the_id() {
    let item = build_item(&meta_at("a", 1), &resolve_ok(&[("fields", "status")]), None);
    let map = item.meta.as_object().expect("object");
    assert!(map.contains_key("run_id"));
    assert!(map.contains_key("status"));
    assert!(!map.contains_key("task"));
}

/// `redacted()` is what strips the webhook signing key, and it is applied at
/// the one place a `RunMeta` becomes JSON on this route.
#[test]
fn an_item_never_carries_the_webhook_secret() {
    let mut meta = meta_at("a", 1);
    meta.callback_url = Some("https://example.invalid/hook".to_string());
    meta.callback_secret = Some("super-secret-signing-key".to_string());

    for resolved in [resolve_ok(&[]), resolve_ok(&[("fields", "status")])] {
        let item = build_item(&meta, &resolved, None);
        let rendered = serde_json::to_string(&item).unwrap();
        assert!(!rendered.contains("super-secret-signing-key"));
    }
}

#[test]
fn highlights_are_omitted_from_the_wire_when_there_are_none() {
    let item = build_item(&meta_at("a", 1), &resolve_ok(&[]), Some(Vec::new()));
    assert!(!serde_json::to_string(&item).unwrap().contains("highlights"));
}

// ─── search: in-memory sources ──────────────────────────────────────────────

#[test]
fn the_meta_source_matches_the_fields_a_user_would_search_for() {
    let mut meta = meta_at("run-abc", 1);
    meta.title = Some("Refactor the retry backoff".to_string());
    meta.error = Some("connection reset".to_string());
    meta.metadata
        .insert("ticket".to_string(), "ENG-4212".to_string());

    let sources = [Source::Meta];
    for needle in [
        "refactor",
        "BACKOFF",
        "connection",
        "ENG-4212",
        "run-abc",
        "do the thing",
    ] {
        assert!(
            matches_query(&meta, needle, &sources),
            "expected {needle} to match"
        );
    }
    assert!(!matches_query(&meta, "nothing-like-this", &sources));
}

/// The signing secret must not be reachable through search either - otherwise
/// it could be confirmed a character at a time.
#[test]
fn the_meta_source_does_not_search_the_webhook_secret() {
    let mut meta = meta_at("a", 1);
    meta.callback_secret = Some("super-secret-signing-key".to_string());
    assert!(!matches_query(&meta, "super-secret", &[Source::Meta]));
}

#[test]
fn the_files_source_matches_tracked_paths_only_when_asked_for() {
    let mut meta = meta_at("a", 1);
    meta.flags.modified_files = vec!["src/retry.rs".to_string()];
    assert!(matches_query(&meta, "retry.rs", &[Source::Files]));
    assert!(!matches_query(&meta, "retry.rs", &[Source::Meta]));
}

#[test]
fn sources_are_ored_together() {
    let mut meta = meta_at("a", 1);
    meta.flags.modified_files = vec!["src/retry.rs".to_string()];
    assert!(matches_query(
        &meta,
        "retry.rs",
        &[Source::Meta, Source::Files]
    ));
}

#[test]
fn highlights_name_the_field_that_matched_and_quote_it() {
    let mut meta = meta_at("a", 1);
    meta.title = Some("Refactor the retry backoff".to_string());
    let out = highlights_for(&meta, "backoff", &[Source::Meta]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].field, "title");
    assert!(out[0].snippet.contains("backoff"));
    assert!(out[0].stage.is_none());
}

#[test]
fn highlights_are_capped_so_one_run_cannot_dominate_a_page() {
    let mut meta = meta_at("aaa", 1);
    meta.title = Some("aaa".to_string());
    meta.error = Some("aaa".to_string());
    meta.model = Some("aaa".to_string());
    for i in 0..20 {
        meta.metadata.insert(format!("k{i}"), "aaa".to_string());
    }
    assert_eq!(
        highlights_for(&meta, "aaa", &[Source::Meta]).len(),
        MAX_HIGHLIGHTS
    );
}

#[test]
fn a_files_highlight_reports_the_matching_path() {
    let mut meta = meta_at("a", 1);
    meta.flags.modified_files = vec!["docs/readme.md".to_string(), "src/retry.rs".to_string()];
    let out = highlights_for(&meta, "retry", &[Source::Files]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].field, "modified_files");
    assert_eq!(out[0].snippet, "src/retry.rs");
}

// ─── search: the scan budget ────────────────────────────────────────────────

/// The guard that keeps `q_in=logs` from becoming a self-inflicted denial of
/// service against a run set nothing prunes.
#[test]
fn a_filesystem_search_stops_after_the_scan_budget_and_says_so() {
    let runs: Vec<RunMeta> = (0..MAX_SEARCH_SCAN + 10)
        .map(|i| meta_at(&format!("run-{i:05}"), i as i64))
        .collect();
    let (kept, truncated) = apply_search(runs, &resolve_ok(&[("q", "x"), ("q_in", "logs")]));
    assert!(
        truncated,
        "the budget must be reported, not silently applied"
    );
    assert!(kept.len() <= MAX_SEARCH_SCAN);
}

/// The in-memory sources cost nothing, so they must not consume the budget -
/// otherwise a plain title search would stop working past 500 runs.
#[test]
fn an_in_memory_search_is_not_budgeted_however_many_runs_there_are() {
    let runs: Vec<RunMeta> = (0..MAX_SEARCH_SCAN + 10)
        .map(|i| meta_at(&format!("run-{i:05}"), i as i64))
        .collect();
    let (kept, truncated) = apply_search(
        runs,
        &resolve_ok(&[("q", "do the thing"), ("q_in", "meta")]),
    );
    assert!(!truncated);
    assert_eq!(kept.len(), MAX_SEARCH_SCAN + 10);
}

#[test]
fn without_a_query_search_keeps_everything_untouched() {
    let runs: Vec<RunMeta> = (0..3).map(|i| meta_at(&format!("r{i}"), i)).collect();
    let (kept, truncated) = apply_search(runs, &resolve_ok(&[]));
    assert_eq!(kept.len(), 3);
    assert!(!truncated);
}

// ─── the handler, over real files ───────────────────────────────────────────

async fn page_of(pairs: &[(&str, &str)]) -> Page<RunItem> {
    list_runs(Query(query(pairs))).await.expect("page").0
}

fn item_ids(page: &Page<RunItem>) -> Vec<String> {
    page.items
        .iter()
        .map(|i| i.meta["run_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn the_handler_pages_and_reports_a_total_and_a_server_time() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-pages", |_d| async move {
        for i in 0..5 {
            create_run(&meta_at(&format!("run-{i}"), 100 + i)).unwrap();
        }

        let page = page_of(&[("limit", "2")]).await;
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, Some(5));
        assert!(page.server_time > 0);
        assert!(page.next_cursor.is_some());
        assert!(!page.scan_truncated);
    })
    .await;
}

/// Every run this API serves carries the two computed spans, and they are
/// projectable like any other key.
///
/// The point is not that the arithmetic is right - `leviath_core::duration` owns
/// that - but that the route serves the keys at all, on the same names, so a
/// client is never handed a run it has to time itself.
#[tokio::test]
async fn every_run_carries_the_computed_age_and_working_spans() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-spans", |_d| async move {
        let mut m = meta_at("run-timed", 100);
        // Launched a long time ago; twelve minutes of that was work, and the
        // clock is stopped, so the figure is exact rather than racing the test.
        m.started_at = chrono::Utc::now().timestamp() - 3_600;
        m.active = Some(leviath_core::run_meta::ActiveClock {
            banked_secs: 720,
            since: None,
        });
        create_run(&m).unwrap();

        let page = page_of(&[]).await;
        let item = &page.items[0].meta;
        assert_eq!(item["working_secs"], 720);
        assert!(
            item["age_secs"].as_u64().unwrap_or(0) >= 3_600,
            "age is the wall-clock span, not the working one: {item}"
        );
        assert_eq!(
            item["started_at"], m.started_at,
            "the raw stamps are still there for a caller that wants them"
        );

        // Projectable: a caller asking for one span gets that and nothing else.
        let page = page_of(&[("fields", "run_id,working_secs")]).await;
        let item = &page.items[0].meta;
        assert_eq!(item["working_secs"], 720);
        assert!(
            item.get("age_secs").is_none(),
            "fields did not ask for it: {item}"
        );
    })
    .await;
}

/// `/api/agents` and `/api/runs` describe the same run the same way.
///
/// They are separate routes with separate handlers, and `/api/agents` used to
/// serialize a bare `RunMeta` - so the same run came back timed on one and
/// untimed on the other, which is the drift this shares one function to prevent.
#[tokio::test]
async fn the_agents_route_times_a_run_the_same_way_the_runs_route_does() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-agents-agree", |_d| async move {
        let mut m = meta_at("run-both", 100);
        m.started_at = chrono::Utc::now().timestamp() - 1_800;
        m.active = Some(leviath_core::run_meta::ActiveClock {
            banked_secs: 300,
            since: None,
        });
        create_run(&m).unwrap();

        let from_runs = page_of(&[]).await.items[0].meta.clone();
        let from_agents =
            super::super::agents::get_agent(axum::extract::Path("run-both".to_string()))
                .await
                .expect("the run is there")
                .0;

        assert_eq!(from_agents["working_secs"], from_runs["working_secs"]);
        assert_eq!(from_agents["working_secs"], 300);
        assert_eq!(from_agents["run_id"], from_runs["run_id"]);
    })
    .await;
}

#[tokio::test]
async fn the_handler_filters_by_the_status_spelling_it_serves() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-status", |_d| async move {
        let mut waiting = meta_at("run-waiting", 1);
        waiting.status = RunStatus::WaitingInput;
        create_run(&waiting).unwrap();
        create_run(&meta_at("run-plain", 2)).unwrap();

        // The serde spelling, which is what a client reads back off the wire.
        let page = page_of(&[("status", "waiting_input")]).await;
        assert_eq!(item_ids(&page), vec!["run-waiting".to_string()]);
    })
    .await;
}

/// The listing pages by runs, and a run's sub-agents are runs, so a console
/// that draws workers nested under the run that started them was paging by a
/// unit it does not display. A real sidebar at `limit=50` got seven visible
/// rows and forty-three workers hanging off them.
#[tokio::test]
async fn parent_none_lists_only_the_runs_nobody_started() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-parent-none", |_d| async move {
        create_run(&meta_at("root-a", 100)).unwrap();
        create_run(&meta_at("root-b", 200)).unwrap();
        for i in 0..8 {
            create_run(&child_of(&format!("worker-{i}"), 300 + i, "root-b")).unwrap();
        }

        let page = page_of(&[("parent", "none")]).await;
        assert_eq!(
            item_ids(&page),
            vec!["root-b".to_string(), "root-a".to_string()]
        );
        // And the count is of what was asked for. `10` here would be counting
        // runs the client will never draw, which is what made it not worth
        // printing.
        assert_eq!(page.total, Some(2));
    })
    .await;
}

/// The other direction: one run's workers, through the same paging, sorting and
/// filtering as everything else. `GET /api/agents/{id}/children` answers this
/// too, in one unpaged array, which a fan-out of two hundred has no windowed
/// form of.
#[tokio::test]
async fn parent_by_id_lists_that_runs_direct_children() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-parent-id", |_d| async move {
        create_run(&meta_at("root", 100)).unwrap();
        create_run(&child_of("worker-1", 200, "root")).unwrap();
        create_run(&child_of("worker-2", 300, "root")).unwrap();
        create_run(&child_of("other-workers-child", 400, "worker-1")).unwrap();

        // Direct children only - the grandchild belongs to `worker-1`.
        let page = page_of(&[("parent", "root")]).await;
        assert_eq!(
            item_ids(&page),
            vec!["worker-2".to_string(), "worker-1".to_string()]
        );
        assert_eq!(page.total, Some(2));

        // A parent that started nothing, or that never existed, is an empty
        // page rather than a 404: a run with no children yet is a normal
        // answer, not a missing resource.
        assert!(page_of(&[("parent", "worker-2")]).await.items.is_empty());
        assert!(page_of(&[("parent", "no-such-run")]).await.items.is_empty());
    })
    .await;
}

/// Omitting it has to leave the route exactly as it was, or every existing
/// caller silently loses its sub-agents.
#[tokio::test]
async fn omitting_parent_still_lists_every_run() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-parent-absent", |_d| async move {
        create_run(&meta_at("root", 100)).unwrap();
        create_run(&child_of("worker", 200, "root")).unwrap();

        assert_eq!(page_of(&[]).await.total, Some(2));
        // An empty value is the same as absent: a client that built its query
        // from an empty box did not mean "runs whose parent is the empty
        // string", which matches nothing.
        assert_eq!(page_of(&[("parent", "")]).await.total, Some(2));
    })
    .await;
}

/// The filters compose, which is the pair a sidebar actually asks for: the
/// top-level runs that are still going. Each is a separate `retain`, so this
/// pins the behaviour rather than the implementation.
#[tokio::test]
async fn parent_and_status_filter_together() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-parent-status", |_d| async move {
        let mut busy_root = meta_at("root-busy", 100);
        busy_root.status = RunStatus::Running;
        create_run(&busy_root).unwrap();

        let mut done_root = meta_at("root-done", 200);
        done_root.status = RunStatus::Complete;
        create_run(&done_root).unwrap();

        // A worker that is running, so a filter on status alone would keep it.
        let mut busy_worker = child_of("worker-busy", 300, "root-busy");
        busy_worker.status = RunStatus::Running;
        create_run(&busy_worker).unwrap();

        let page = page_of(&[("parent", "none"), ("status", "running")]).await;
        assert_eq!(item_ids(&page), vec!["root-busy".to_string()]);
        assert_eq!(page.total, Some(1));

        // Several statuses at once still work alongside it.
        let page = page_of(&[("parent", "none"), ("status", "running,complete")]).await;
        assert_eq!(
            item_ids(&page),
            vec!["root-done".to_string(), "root-busy".to_string()]
        );
    })
    .await;
}

/// Every status a run can be in is selectable by the word the API hands back,
/// whatever the spelling. A filter that silently matches nothing is worse than
/// one that errors, and the two multi-word states are where it would happen.
#[tokio::test]
async fn every_status_is_selectable_by_the_word_the_api_returns() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-every-status", |_d| async move {
        let states = [
            (RunStatus::Starting, "starting"),
            (RunStatus::Running, "running"),
            (RunStatus::WaitingInput, "waiting_input"),
            (RunStatus::Paused, "paused"),
            (RunStatus::Complete, "complete"),
            (RunStatus::CompleteInteractive, "complete_interactive"),
            (RunStatus::Error, "error"),
            (RunStatus::Cancelled, "cancelled"),
        ];
        for (i, (status, word)) in states.iter().enumerate() {
            let mut meta = meta_at(&format!("run-{word}"), i as i64);
            meta.status = status.clone();
            create_run(&meta).unwrap();
        }

        for (_, word) in states {
            assert_eq!(
                item_ids(&page_of(&[("status", word)]).await),
                vec![format!("run-{word}")],
                "filtering by {word}"
            );
        }

        // And the loose spellings of the two that have more than one word, so a
        // status taken from any response can be fed straight back as a filter.
        for spelling in ["waitinginput", "Waiting-Input", "WaitingInput"] {
            assert_eq!(
                item_ids(&page_of(&[("status", spelling)]).await),
                vec!["run-waiting_input".to_string()],
                "filtering by {spelling}"
            );
        }
    })
    .await;
}

/// The batch fetch that replaces N separate `GET /api/agents/{id}` calls.
#[tokio::test]
async fn ids_fetches_exactly_those_runs_and_reports_the_ones_that_are_gone() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-ids", |_d| async move {
        create_run(&meta_at("run-a", 1)).unwrap();
        create_run(&meta_at("run-b", 2)).unwrap();

        let page = page_of(&[("ids", "run-b,run-a,run-vanished")]).await;
        // In the order asked for, not in sort order.
        assert_eq!(
            item_ids(&page),
            vec!["run-b".to_string(), "run-a".to_string()]
        );
        assert_eq!(page.missing, vec!["run-vanished".to_string()]);
        assert!(page.next_cursor.is_none());
    })
    .await;
}

#[tokio::test]
async fn since_filters_on_the_sorted_field_inclusively() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-since", |_d| async move {
        create_run(&meta_at("run-old", 100)).unwrap();
        create_run(&meta_at("run-edge", 200)).unwrap();
        create_run(&meta_at("run-new", 300)).unwrap();

        // Inclusive, so the run exactly on the boundary is re-delivered rather
        // than lost - the safe direction at seconds granularity.
        assert_eq!(
            item_ids(&page_of(&[("since", "200")]).await),
            vec!["run-new".to_string(), "run-edge".to_string()]
        );
    })
    .await;
}

/// The part that cannot be done in the browser: the console never holds a
/// run's transcript, which is the whole reason search moved to the server.
#[tokio::test]
async fn a_deep_search_finds_text_only_present_in_a_stage_log_and_says_where() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-deep", |_d| async move {
        create_run(&meta_at("run-deep", 1)).unwrap();
        crate::runstate::write_stages_index(
            "run-deep",
            &[leviath_core::run_meta::StageRecord {
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                ..leviath_core::run_meta::StageRecord::new("review".to_string(), 0)
            }],
        )
        .unwrap();
        crate::runstate::append_stage_output("run-deep", 0, "the mitochondria is the powerhouse");

        // Invisible at the default depth: this text is in no metadata field.
        assert!(page_of(&[("q", "mitochondria")]).await.items.is_empty());

        // Opting in finds it, and the highlight names the stage so the client
        // can go fetch that log next.
        let page = page_of(&[("q", "mitochondria"), ("q_in", "logs")]).await;
        assert_eq!(page.items.len(), 1);
        let highlight = &page.items[0].highlights[0];
        assert_eq!(highlight.field, "logs.output");
        assert_eq!(highlight.stage, Some(0));
        assert!(highlight.snippet.contains("mitochondria"));
    })
    .await;
}

/// Write a journal for `run_id` whose context carries `content`, and whose
/// metadata carries a webhook secret.
fn plant_journal(run_id: &str, content: &str, secret: Option<&str>) {
    use leviath_core::run_archive::{self, RunIdentity, RunRecord};
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

    let mut meta = meta_at(run_id, 1);
    meta.callback_secret = secret.map(str::to_string);

    let mut buf = Vec::new();
    run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
    run_archive::write_record(
        &mut buf,
        &RunRecord::Header {
            identity: RunIdentity {
                run_id: run_id.to_string(),
                machine_id: "m".to_string(),
                world_id: "w".to_string(),
                created_at: 0,
            },
            meta: Box::new(meta),
        },
    )
    .unwrap();
    run_archive::write_record(
        &mut buf,
        &RunRecord::ContextCheckpoint {
            snapshot: ContextSnapshot {
                stage_name: "work".to_string(),
                total_tokens: 1,
                max_tokens: 100,
                regions: vec![RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1,
                    max_tokens: 100,
                    entries: vec![RegionEntrySnapshot {
                        content: content.to_string(),
                        tokens: 1,
                        kind: leviath_core::region::EntryKind::Text,
                        metadata: None,
                        key: None,
                        taint: leviath_core::taint::TaintLevel::default(),
                        reasoning: None,
                    }],
                    description: None,
                }],
            },
            at: 2,
        },
    )
    .unwrap();
    std::fs::write(crate::runstate::run_dir(run_id).join("run.lvr"), &buf).unwrap();
}

/// Regression test for a gap found by running this against real journals: runs
/// matched on `q_in=journal` and came back with no highlight at all, because
/// only tool batches were being inspected while the text lived in the journal's
/// context records. A result with no explanation is the thing server-side
/// search exists to avoid.
#[tokio::test]
async fn a_journal_match_in_a_context_record_still_explains_itself() {
    crate::runstate::with_isolated_runs_dir_async("runs-journal-context", |_d| async move {
        create_run(&meta_at("run-j", 1)).unwrap();
        plant_journal("run-j", "the codex entry mentions xylophone here", None);

        let page = page_of(&[("q", "xylophone"), ("q_in", "journal")]).await;
        assert_eq!(page.items.len(), 1);
        let highlights = &page.items[0].highlights;
        assert!(
            !highlights.is_empty(),
            "a match with no highlight is the bug"
        );
        assert_eq!(highlights[0].field, "journal.context.system");
        assert!(highlights[0].snippet.contains("xylophone"));
    })
    .await;
}

/// The journal stores `RunMeta` whole, secret included, so the highlighter must
/// never cut a snippet from those bytes. Phase one scans the raw file and so
/// *can* match there - the run may come back - but nothing it returns may echo
/// the secret.
#[tokio::test]
async fn searching_the_journal_never_echoes_the_webhook_secret() {
    crate::runstate::with_isolated_runs_dir_async("runs-journal-secret", |_d| async move {
        create_run(&meta_at("run-s", 1)).unwrap();
        plant_journal(
            "run-s",
            "ordinary content",
            Some("super-secret-signing-key"),
        );

        for source in ["journal", "meta", "context"] {
            let page = page_of(&[("q", "super-secret-signing-key"), ("q_in", source)]).await;
            let rendered = serde_json::to_string(&page.items).unwrap();
            assert!(
                !rendered.contains("super-secret-signing-key"),
                "q_in={source} echoed the signing key"
            );
        }
    })
    .await;
}

/// A journal exercising every record kind the highlighter reads: a tool call,
/// an appended region, a replaced region, and a full checkpoint.
fn plant_rich_journal(run_id: &str) {
    use leviath_core::run_archive::{
        self, ContextDelta, RegionDelta, RunIdentity, RunRecord, ToolCallRecord,
    };
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

    fn entry(content: &str) -> RegionEntrySnapshot {
        RegionEntrySnapshot {
            content: content.to_string(),
            tokens: 1,
            kind: leviath_core::region::EntryKind::Text,
            metadata: None,
            key: None,
            taint: leviath_core::taint::TaintLevel::default(),
            reasoning: None,
        }
    }
    fn region(name: &str, content: &str) -> RegionSnapshot {
        RegionSnapshot {
            name: name.to_string(),
            kind: "pinned".to_string(),
            current_tokens: 1,
            max_tokens: 100,
            entries: vec![entry(content)],
            description: None,
        }
    }
    fn snapshot(regions: Vec<RegionSnapshot>) -> ContextSnapshot {
        ContextSnapshot {
            stage_name: "work".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions,
        }
    }
    fn delta(regions: Vec<RegionDelta>) -> ContextDelta {
        ContextDelta {
            stage_name: "work".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions,
        }
    }

    let mut buf = Vec::new();
    run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
    let write = |buf: &mut Vec<u8>, record: &RunRecord| {
        run_archive::write_record(buf, record).unwrap();
    };
    write(
        &mut buf,
        &RunRecord::Header {
            identity: RunIdentity {
                run_id: run_id.to_string(),
                machine_id: "m".to_string(),
                world_id: "w".to_string(),
                created_at: 0,
            },
            meta: Box::new(meta_at(run_id, 1)),
        },
    );
    write(
        &mut buf,
        &RunRecord::ToolBatch {
            calls: vec![ToolCallRecord {
                id: "c1".to_string(),
                name: "write_file".to_string(),
                arguments: r#"{"path":"toolneedle.rs"}"#.to_string(),
                result: Some("wrote resultneedle".to_string()),
                thought_signature: None,
            }],
            at: 2,
            stage_index: 3,
            iteration: 0,
            response: String::new(),
        },
    );
    write(
        &mut buf,
        &RunRecord::ContextCheckpoint {
            snapshot: snapshot(vec![region("system", "checkpointneedle here")]),
            at: 3,
        },
    );
    write(
        &mut buf,
        &RunRecord::Progress {
            meta: Box::new(meta_at(run_id, 1)),
            delta: delta(vec![RegionDelta::Append {
                name: "conversation".to_string(),
                entries: vec![entry("appendneedle here")],
                current_tokens: 2,
            }]),
            at: 4,
        },
    );
    write(
        &mut buf,
        &RunRecord::ContextDiff {
            delta: delta(vec![RegionDelta::Set(region("scratch", "setneedle here"))]),
            at: 5,
        },
    );
    // Arms that carry no text of their own, so the highlighter must skip them
    // rather than treat them as a miss.
    write(
        &mut buf,
        &RunRecord::ContextDiff {
            delta: delta(vec![
                RegionDelta::Clear {
                    name: "conversation".to_string(),
                },
                RegionDelta::Remove {
                    name: "scratch".to_string(),
                },
            ]),
            at: 6,
        },
    );
    write(
        &mut buf,
        &RunRecord::Checkpoint {
            meta: Box::new(meta_at(run_id, 1)),
            context: snapshot(vec![region("final", "checkneedle here")]),
            at: 7,
        },
    );
    std::fs::write(crate::runstate::run_dir(run_id).join("run.lvr"), &buf).unwrap();
}

/// Each record kind that carries text has to be reachable, or a match in it is
/// a result the user cannot account for.
#[tokio::test]
async fn journal_highlights_name_the_record_the_match_came_from() {
    crate::runstate::with_isolated_runs_dir_async("runs-journal-kinds", |_d| async move {
        create_run(&meta_at("run-rich", 1)).unwrap();
        plant_rich_journal("run-rich");

        // A tool call reports the tool and its stage, so a client can jump
        // straight to that stage's log.
        for (needle, field, stage) in [
            ("toolneedle", "journal.tool.write_file", Some(3)),
            ("resultneedle", "journal.tool.write_file", Some(3)),
        ] {
            let page = page_of(&[("q", needle), ("q_in", "journal")]).await;
            assert_eq!(page.items.len(), 1, "{needle} should match");
            assert_eq!(page.items[0].highlights[0].field, field);
            assert_eq!(page.items[0].highlights[0].stage, stage);
        }

        // Context text is named by the region it lived in, whether it arrived
        // as a checkpoint, an append, a replacement, or a full checkpoint.
        for (needle, field) in [
            ("checkpointneedle", "journal.context.system"),
            ("appendneedle", "journal.context.conversation"),
            ("setneedle", "journal.context.scratch"),
            ("checkneedle", "journal.context.final"),
        ] {
            let page = page_of(&[("q", needle), ("q_in", "journal")]).await;
            assert_eq!(page.items.len(), 1, "{needle} should match");
            let highlight = &page.items[0].highlights[0];
            assert_eq!(highlight.field, field);
            assert!(highlight.snippet.contains(needle));
        }
    })
    .await;
}

/// The operational stream carries tool activity and errors, which is often
/// exactly what someone is looking for.
#[tokio::test]
async fn a_log_search_can_match_the_operational_stream() {
    crate::runstate::with_isolated_runs_dir_async("runs-logs-operational", |_d| async move {
        create_run(&meta_at("run-ops", 1)).unwrap();
        crate::runstate::write_stages_index(
            "run-ops",
            &[leviath_core::run_meta::StageRecord {
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                ..leviath_core::run_meta::StageRecord::new("work".to_string(), 0)
            }],
        )
        .unwrap();
        crate::runstate::append_stage_output("run-ops", 0, "ordinary assistant text");
        crate::runstate::append_stage_log("run-ops", 0, "[error] opsneedle exploded");

        let page = page_of(&[("q", "opsneedle"), ("q_in", "logs")]).await;
        assert_eq!(page.items.len(), 1);
        let highlight = &page.items[0].highlights[0];
        assert_eq!(highlight.field, "logs.operational");
        assert_eq!(highlight.stage, Some(0));
        assert!(highlight.snippet.contains("opsneedle"));
    })
    .await;
}

/// One run matching in many places must not crowd out the rest of the page.
#[tokio::test]
async fn journal_highlights_stop_at_the_cap() {
    crate::runstate::with_isolated_runs_dir_async("runs-journal-cap", |_d| async move {
        create_run(&meta_at("run-cap", 1)).unwrap();
        plant_rich_journal("run-cap");

        // "needle" appears in every planted record.
        let page = page_of(&[("q", "needle"), ("q_in", "journal")]).await;
        assert_eq!(page.items.len(), 1);
        assert!(page.items[0].highlights.len() <= MAX_HIGHLIGHTS);
    })
    .await;
}

/// A match in the run's current context window, named by the region it is in.
#[tokio::test]
async fn a_context_search_names_the_region_that_matched() {
    crate::runstate::with_isolated_runs_dir_async("runs-context-region", |_d| async move {
        create_run(&meta_at("run-ctx", 1)).unwrap();
        crate::runstate::write_context_snapshot(
            "run-ctx",
            &leviath_core::run_meta::ContextSnapshot {
                stage_name: "work".to_string(),
                total_tokens: 1,
                max_tokens: 100,
                regions: vec![leviath_core::run_meta::RegionSnapshot {
                    name: "working_memory".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1,
                    max_tokens: 100,
                    entries: vec![leviath_core::run_meta::RegionEntrySnapshot {
                        content: "the plan mentions ctxneedle somewhere".to_string(),
                        tokens: 1,
                        kind: leviath_core::region::EntryKind::Text,
                        metadata: None,
                        key: None,
                        taint: leviath_core::taint::TaintLevel::default(),
                        reasoning: None,
                    }],
                    description: None,
                }],
            },
        )
        .unwrap();

        let page = page_of(&[("q", "ctxneedle"), ("q_in", "context")]).await;
        assert_eq!(page.items.len(), 1);
        let highlight = &page.items[0].highlights[0];
        assert_eq!(highlight.field, "context.working_memory");
        assert!(highlight.snippet.contains("ctxneedle"));
    })
    .await;
}

/// Once a run has filled its highlight budget from one source, the remaining
/// sources must stop rather than keep reading files for output that would be
/// discarded. Also covers each deep source finding nothing to read at all.
#[tokio::test]
async fn deep_sources_stop_once_the_highlight_budget_is_full() {
    crate::runstate::with_isolated_runs_dir_async("runs-budget-full", |_d| async move {
        let mut meta = meta_at("run-full", 1);
        // Enough metadata matches to fill the budget before any file is read.
        meta.title = Some("aaa".to_string());
        meta.error = Some("aaa".to_string());
        meta.model = Some("aaa".to_string());
        for i in 0..10 {
            meta.metadata.insert(format!("k{i}"), "aaa".to_string());
        }
        create_run(&meta).unwrap();
        // Deliberately no context.json, no stages and no journal, so each deep
        // source also exercises its "nothing here" path.
        crate::runstate::write_stages_index(
            "run-full",
            &[leviath_core::run_meta::StageRecord {
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                ..leviath_core::run_meta::StageRecord::new("work".to_string(), 0)
            }],
        )
        .unwrap();

        let page = page_of(&[("q", "aaa"), ("q_in", "meta,context,logs,journal")]).await;
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].highlights.len(), MAX_HIGHLIGHTS);
    })
    .await;
}

/// The descending walk is covered above; ascending mints its own cursors, and
/// a cursor minted for the wrong direction is unusable.
#[tokio::test]
async fn an_ascending_page_mints_a_usable_cursor() {
    crate::runstate::with_isolated_runs_dir_async("runs-asc-cursor", |_d| async move {
        for i in 0..4 {
            create_run(&meta_at(&format!("run-{i}"), 100 + i)).unwrap();
        }

        let first = page_of(&[("limit", "2"), ("order", "asc")]).await;
        assert_eq!(
            item_ids(&first),
            vec!["run-0".to_string(), "run-1".to_string()]
        );
        let cursor = first.next_cursor.expect("more pages");

        let second = page_of(&[("limit", "2"), ("order", "asc"), ("cursor", &cursor)]).await;
        assert_eq!(
            item_ids(&second),
            vec!["run-2".to_string(), "run-3".to_string()]
        );
        assert!(second.next_cursor.is_none());
    })
    .await;
}

/// Every sort key has to survive a cursor round trip, since the cursor records
/// which key it was minted for and refuses to be used against another.
#[tokio::test]
async fn each_sort_key_pages_with_its_own_cursor() {
    crate::runstate::with_isolated_runs_dir_async("runs-sort-keys", |_d| async move {
        for i in 0..4 {
            let mut meta = meta_at(&format!("run-{i}"), 100 + i);
            meta.updated_at = 200 + i;
            meta.last_progress_at = Some(300 + i);
            create_run(&meta).unwrap();
        }

        for sort in ["started_at", "updated_at", "last_progress_at"] {
            let first = page_of(&[("limit", "2"), ("sort", sort)]).await;
            assert_eq!(first.items.len(), 2, "{sort} first page");
            let first_ids = item_ids(&first);
            let cursor = first
                .next_cursor
                .clone()
                .unwrap_or_else(|| panic!("{sort} should have a second page"));
            let second = page_of(&[("limit", "2"), ("sort", sort), ("cursor", &cursor)]).await;
            assert_eq!(second.items.len(), 2, "{sort} second page");
            // No overlap between the two pages.
            for id in item_ids(&second) {
                assert!(!first_ids.contains(&id), "{sort} repeated {id}");
            }
        }
    })
    .await;
}

/// A source that is asked about but finds nothing must simply contribute no
/// highlight, rather than suppressing the ones that did match.
#[tokio::test]
async fn a_source_with_nothing_to_say_does_not_suppress_the_others() {
    crate::runstate::with_isolated_runs_dir_async("runs-quiet-source", |_d| async move {
        let mut meta = meta_at("run-quiet", 1);
        meta.title = Some("quietneedle in the title".to_string());
        meta.flags.modified_files = vec!["unrelated.rs".to_string()];
        create_run(&meta).unwrap();

        // Matches only via meta, but every other source is asked too - and
        // none of them has a file to read.
        let page = page_of(&[
            ("q", "quietneedle"),
            ("q_in", "meta,files,context,logs,journal"),
        ])
        .await;
        assert_eq!(page.items.len(), 1);
        let highlights = &page.items[0].highlights;
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].field, "title");
    })
    .await;
}

/// A run whose files exist but do not match, so the deep sources are read and
/// come back empty rather than being skipped.
#[tokio::test]
async fn deep_sources_that_read_files_and_find_nothing_are_quiet() {
    crate::runstate::with_isolated_runs_dir_async("runs-quiet-deep", |_d| async move {
        let mut meta = meta_at("run-deepquiet", 1);
        meta.title = Some("deepquietneedle".to_string());
        create_run(&meta).unwrap();

        // All three deep sources have something to read, none of it matching.
        crate::runstate::write_context_snapshot(
            "run-deepquiet",
            &leviath_core::run_meta::ContextSnapshot {
                stage_name: "work".to_string(),
                total_tokens: 1,
                max_tokens: 100,
                regions: vec![leviath_core::run_meta::RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1,
                    max_tokens: 100,
                    entries: vec![leviath_core::run_meta::RegionEntrySnapshot {
                        content: "nothing of interest".to_string(),
                        tokens: 1,
                        kind: leviath_core::region::EntryKind::Text,
                        metadata: None,
                        key: None,
                        taint: leviath_core::taint::TaintLevel::default(),
                        reasoning: None,
                    }],
                    description: None,
                }],
            },
        )
        .unwrap();
        crate::runstate::write_stages_index(
            "run-deepquiet",
            &[leviath_core::run_meta::StageRecord {
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                ..leviath_core::run_meta::StageRecord::new("work".to_string(), 0)
            }],
        )
        .unwrap();
        crate::runstate::append_stage_output("run-deepquiet", 0, "ordinary output");
        crate::runstate::append_stage_log("run-deepquiet", 0, "[tool] ordinary");
        plant_rich_journal("run-deepquiet");

        let page = page_of(&[
            ("q", "deepquietneedle"),
            ("q_in", "meta,context,logs,journal"),
        ])
        .await;
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].highlights.len(), 1);
        assert_eq!(page.items[0].highlights[0].field, "title");
    })
    .await;
}

#[tokio::test]
async fn a_bad_request_is_reported_rather_than_served() {
    crate::runstate::with_isolated_runs_dir_async("runs-handler-bad", |_d| async move {
        let (status, _) = list_runs(Query(query(&[("sort", "nonsense")])))
            .await
            .expect_err("should be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

// ─── delete ─────────────────────────────────────────────────────────────────

/// Build a `DeleteRunsQuery` through axum's extractor, for the same reason
/// `query` above does: a struct shape the wire never produces is not a test.
fn delete_query(pairs: &[(&str, &str)]) -> DeleteRunsQuery {
    let encoded = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let uri: axum::http::Uri = format!("http://test/api/runs?{encoded}")
        .parse()
        .expect("uri parses");
    Query::<DeleteRunsQuery>::try_from_uri(&uri)
        .expect("query parses")
        .0
}

/// The single-route query, through the extractor for the same reason as above.
fn force(on: bool) -> Query<DeleteRunQuery> {
    let uri: axum::http::Uri = format!("http://test/api/runs/x?force={on}")
        .parse()
        .expect("uri parses");
    Query::<DeleteRunQuery>::try_from_uri(&uri).expect("query parses")
}

fn finished(id: &str, updated_at: i64) -> RunMeta {
    let mut meta = meta_at(id, updated_at);
    meta.status = RunStatus::Complete;
    meta.updated_at = updated_at;
    meta
}

#[tokio::test]
async fn deleting_a_finished_run_removes_it_from_disk() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-one", |_d| async move {
        create_run(&finished("run-done", 1)).unwrap();
        assert!(crate::runstate::run_dir("run-done").exists());

        let code = delete_run(AxumPath("run-done".to_string()), force(false))
            .await
            .expect("the delete succeeds");

        assert_eq!(code, StatusCode::NO_CONTENT);
        // The directory, not just the listing entry: a delete that left the
        // transcript behind would be the "hides it locally" bug with extra
        // steps.
        assert!(!crate::runstate::run_dir("run-done").exists());
        assert!(crate::runstate::list_runs().is_empty());
    })
    .await;
}

#[tokio::test]
async fn deleting_a_live_run_is_refused_rather_than_done_behind_the_agent() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-live", |_d| async move {
        create_run(&meta_at("run-live", 1)).unwrap();

        let (code, body) = delete_run(AxumPath("run-live".to_string()), force(false))
            .await
            .expect_err("a live run is refused");

        assert_eq!(code, StatusCode::CONFLICT);
        // Says what to do about it, not just that it failed.
        assert!(body.0.error.contains("cancel it"));
        assert!(crate::runstate::run_dir("run-live").exists());
    })
    .await;
}

/// A repeat of a delete whose response was lost has to be readable as "already
/// gone" rather than as a failure.
#[tokio::test]
async fn deleting_a_run_that_is_already_gone_is_a_404() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-missing", |_d| async move {
        let (code, _) = delete_run(AxumPath("run-never".to_string()), force(false))
            .await
            .expect_err("a missing run is a 404");
        assert_eq!(code, StatusCode::NOT_FOUND);
    })
    .await;
}

/// `run_dir` maps an unsafe id onto a path that cannot exist, so a traversal
/// arrives as an ordinary miss. Asserted here because this is the one route
/// where being wrong removes a directory.
#[tokio::test]
async fn a_traversing_run_id_deletes_nothing() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-traversal", |dir| async move {
        let victim = dir.join("keep-me");
        std::fs::create_dir_all(&victim).unwrap();

        let (code, _) = delete_run(AxumPath("../keep-me".to_string()), force(false))
            .await
            .expect_err("a traversing id finds nothing");

        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(victim.exists());
    })
    .await;
}

/// A record that will not parse says nothing about whether the run is finished,
/// and "cannot read it" must not quietly read as "finished" - that is what a
/// *live* run looks like to a binary whose `RunMeta` has moved on, and deleting
/// one is exactly what this route refuses to do for a run it can see is live.
///
/// It is still deletable on request, because `list_runs` skips it and refusing
/// outright would leave it both invisible and permanent.
#[tokio::test]
async fn a_run_with_unreadable_metadata_is_refused_until_the_caller_forces_it() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-corrupt", |_d| async move {
        create_run(&finished("run-corrupt", 1)).unwrap();
        std::fs::write(
            crate::runstate::run_dir("run-corrupt").join("meta.json"),
            "{ not json",
        )
        .unwrap();

        let (code, body) = delete_run(AxumPath("run-corrupt".to_string()), force(false))
            .await
            .expect_err("an unreadable record is not proof the run finished");
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(body.0.error.contains("force=true"), "{}", body.0.error);
        assert!(crate::runstate::run_dir("run-corrupt").exists());

        let code = delete_run(AxumPath("run-corrupt".to_string()), force(true))
            .await
            .expect("forcing it works");
        assert_eq!(code, StatusCode::NO_CONTENT);
        assert!(!crate::runstate::run_dir("run-corrupt").exists());
    })
    .await;
}

/// `force` is not offered on the bulk route at all, and a sweep must not pick up
/// an unreadable run as collateral.
#[tokio::test]
async fn a_bulk_sweep_never_forces_an_unreadable_run() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-bulk-corrupt", |_d| async move {
        create_run(&finished("run-corrupt", 1)).unwrap();
        std::fs::write(
            crate::runstate::run_dir("run-corrupt").join("meta.json"),
            "{ not json",
        )
        .unwrap();

        let resp = delete_runs(Query(delete_query(&[("ids", "run-corrupt")])))
            .await
            .expect("the sweep runs");

        assert!(resp.deleted.is_empty());
        assert!(resp.skipped[0].reason.contains("force=true"));
        assert!(crate::runstate::run_dir("run-corrupt").exists());
    })
    .await;
}

#[tokio::test]
async fn a_bulk_delete_by_age_takes_the_old_finished_runs_and_leaves_the_rest() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-before", |_d| async move {
        create_run(&finished("run-ancient", 100)).unwrap();
        create_run(&finished("run-recent", 300)).unwrap();
        let mut live = meta_at("run-live", 100);
        live.status = RunStatus::Running;
        create_run(&live).unwrap();

        let resp = delete_runs(Query(delete_query(&[("before", "200")])))
            .await
            .expect("the sweep runs");

        assert_eq!(resp.deleted, vec!["run-ancient".to_string()]);
        // The live run is old enough by the clock and is still not swept: it is
        // filtered out at selection, so it is not reported as a skip either.
        assert!(resp.skipped.is_empty());
        assert!(crate::runstate::run_dir("run-live").exists());
        assert!(crate::runstate::run_dir("run-recent").exists());
    })
    .await;
}

/// Partial success is the normal outcome, and the caller has to be able to tell
/// the two kinds of non-deletion apart.
#[tokio::test]
async fn a_bulk_delete_by_id_reports_a_verdict_for_every_run_it_was_given() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-ids", |_d| async move {
        create_run(&finished("run-done", 1)).unwrap();
        create_run(&meta_at("run-live", 2)).unwrap();

        let resp = delete_runs(Query(delete_query(&[(
            "ids",
            "run-done,run-live,run-gone",
        )])))
        .await
        .expect("the sweep runs");

        assert_eq!(resp.deleted, vec!["run-done".to_string()]);
        let reasons: Vec<(&str, &str)> = resp
            .skipped
            .iter()
            .map(|s| (s.id.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(reasons.len(), 2);
        assert_eq!(reasons[0].0, "run-live");
        assert!(reasons[0].1.contains("cancel it"));
        assert_eq!(reasons[1].0, "run-gone");
        assert!(reasons[1].1.contains("not found"));
    })
    .await;
}

// ─── delete: sub-agent trees ────────────────────────────────────────────────

/// A finished run recorded as `parent`'s sub-agent, which is how a fan-out
/// worker or a `sub_agent` spawn appears on disk.
fn finished_child(id: &str, parent: &str, updated_at: i64) -> RunMeta {
    let mut meta = finished(id, updated_at);
    meta.parent_run_id = Some(parent.to_string());
    meta
}

/// A sub-agent run exists only because its parent spawned it, so forgetting the
/// parent forgets the children. Left behind they were not merely stale rows:
/// the dashboard treats a run whose parent is absent as a root, so a delete
/// promoted them to the top of the list instead of clearing them.
#[tokio::test]
async fn deleting_a_parent_takes_its_sub_agent_runs_with_it() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-tree", |_d| async move {
        create_run(&finished("run-parent", 1)).unwrap();
        create_run(&finished_child("run-kid", "run-parent", 2)).unwrap();
        create_run(&finished_child("run-grandkid", "run-kid", 3)).unwrap();
        // A run of its own, to show the delete is scoped to the tree.
        create_run(&finished("run-other", 4)).unwrap();

        let code = delete_run(AxumPath("run-parent".to_string()), force(false))
            .await
            .expect("the delete succeeds");

        assert_eq!(code, StatusCode::NO_CONTENT);
        assert!(!crate::runstate::run_dir("run-parent").exists());
        assert!(!crate::runstate::run_dir("run-kid").exists());
        assert!(!crate::runstate::run_dir("run-grandkid").exists());
        assert!(crate::runstate::run_dir("run-other").exists());
    })
    .await;
}

/// The tree is walked downwards only. Deleting one worker out of a fan-out is
/// an ordinary thing to do and must not take the run that started it, or the
/// workers beside it.
#[tokio::test]
async fn deleting_a_sub_agent_run_leaves_its_parent_and_siblings() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-child-only", |_d| async move {
        create_run(&finished("run-parent", 1)).unwrap();
        create_run(&finished_child("run-kid-a", "run-parent", 2)).unwrap();
        create_run(&finished_child("run-kid-b", "run-parent", 3)).unwrap();

        let code = delete_run(AxumPath("run-kid-a".to_string()), force(false))
            .await
            .expect("the delete succeeds");

        assert_eq!(code, StatusCode::NO_CONTENT);
        assert!(!crate::runstate::run_dir("run-kid-a").exists());
        assert!(crate::runstate::run_dir("run-parent").exists());
        assert!(crate::runstate::run_dir("run-kid-b").exists());
    })
    .await;
}

/// Half a tree is not a state anything downstream knows how to read, so a live
/// sub-agent refuses the whole delete - and the reason names it, because
/// "cancel it before deleting it" about a run the caller never mentioned is
/// unactionable on its own.
#[tokio::test]
async fn a_live_sub_agent_refuses_its_parents_delete_and_says_which_run_it_is() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-live-child", |_d| async move {
        create_run(&finished("run-parent", 1)).unwrap();
        let mut live = meta_at("run-kid", 2);
        live.parent_run_id = Some("run-parent".to_string());
        create_run(&live).unwrap();

        let (code, body) = delete_run(AxumPath("run-parent".to_string()), force(false))
            .await
            .expect_err("a live sub-agent is refused");

        assert_eq!(code, StatusCode::CONFLICT);
        assert!(body.0.error.contains("run-kid"), "{}", body.0.error);
        assert!(body.0.error.contains("sub-agent run"), "{}", body.0.error);
        // Nothing was taken on the way to the refusal.
        assert!(crate::runstate::run_dir("run-parent").exists());
        assert!(crate::runstate::run_dir("run-kid").exists());
    })
    .await;
}

/// The bulk route deletes trees too, and reports the sub-agents by id: they are
/// runs that are now gone, and a caller reconciling its own list needs to know.
#[tokio::test]
async fn a_bulk_delete_reports_the_sub_agent_runs_it_removed() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-bulk-tree", |_d| async move {
        create_run(&finished("run-parent", 1)).unwrap();
        create_run(&finished_child("run-kid", "run-parent", 2)).unwrap();

        let resp = delete_runs(Query(delete_query(&[("ids", "run-parent")])))
            .await
            .expect("the sweep runs");

        // Deepest first, which is the order they were removed in.
        assert_eq!(
            resp.deleted,
            vec!["run-kid".to_string(), "run-parent".to_string()]
        );
        assert!(resp.skipped.is_empty());
        assert!(crate::runstate::list_runs().is_empty());
    })
    .await;
}

/// A sweep names a parent and its child independently all the time - `before`
/// selects both, and a console sending marked rows sends both. The second
/// mention is of a run this same request has already removed, which is a
/// deletion, not the 404 a re-check would report.
#[tokio::test]
async fn a_bulk_delete_naming_a_parent_and_its_child_counts_each_once() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-bulk-dup", |_d| async move {
        create_run(&finished("run-parent", 1)).unwrap();
        create_run(&finished_child("run-kid", "run-parent", 2)).unwrap();

        let resp = delete_runs(Query(delete_query(&[("ids", "run-parent,run-kid")])))
            .await
            .expect("the sweep runs");

        assert_eq!(
            resp.deleted,
            vec!["run-kid".to_string(), "run-parent".to_string()]
        );
        assert!(
            resp.skipped.is_empty(),
            "the child is deleted, not skipped: {:?}",
            resp.skipped
        );
    })
    .await;
}

/// Far likelier to be a client that failed to build its query than an operator
/// asking to erase the machine's history.
#[tokio::test]
async fn a_bulk_delete_with_no_predicate_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("runs-delete-nothing", |_d| async move {
        create_run(&finished("run-done", 1)).unwrap();

        let (code, body) = delete_runs(Query(delete_query(&[])))
            .await
            .expect_err("a predicate is required");

        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.0.error.contains("before"));
        assert!(crate::runstate::run_dir("run-done").exists());
    })
    .await;
}

#[tokio::test]
async fn a_bulk_delete_naming_more_runs_than_the_cap_is_refused() {
    let many = (0..MAX_IDS + 1)
        .map(|i| format!("run-{i}"))
        .collect::<Vec<_>>()
        .join(",");

    let (code, body) = delete_runs(Query(delete_query(&[("ids", &many)])))
        .await
        .expect_err("over the cap");

    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(body.0.error.contains("at most"));
}

/// The failing arm of `remove_run`. Reached with a run directory that exists
/// and is terminal but cannot be removed, which on unix means a parent that is
/// read-only.
#[cfg(unix)]
#[tokio::test]
async fn a_run_that_cannot_be_removed_reports_the_failure() {
    use std::os::unix::fs::PermissionsExt;

    crate::runstate::with_isolated_runs_dir_async("runs-delete-locked", |_d| async move {
        create_run(&finished("run-stuck", 1)).unwrap();
        // The runs dir itself, not the temp root above it: removing a directory
        // needs write permission on its own parent.
        let dir = crate::runstate::runs_dir();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&dir, perms).unwrap();

        let outcome = delete_run(AxumPath("run-stuck".to_string()), force(false)).await;

        // Restored before asserting, so a failed assert cannot leave an
        // undeletable directory behind for the next run of the suite.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&dir, perms).unwrap();

        let (code, body) = outcome.expect_err("the removal fails");
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.0.error.contains("Failed to delete"));
    })
    .await;
}

/// The Windows twin.
///
/// Windows has no equivalent of the Unix "write permission on the parent
/// directory governs unlinking", and marking a file inside read-only does not
/// help either: `remove_dir_all` clears that attribute and deletes anyway. A
/// *sharing violation* does block it - but only from a handle opened with no
/// share mode. A plain `File::open` shares delete access, so holding one lets
/// the removal succeed, which is how the first version of this test passed
/// locally and answered 204 on CI.
///
/// The file is also created through that exclusive handle rather than written
/// and reopened. Reopening leaves a window in which Defender or the indexer has
/// the just-written file open, and then it is *our* exclusive open that takes
/// the sharing violation - a known flake here, documented on
/// `delete_blueprint_removal_failure_returns_500_windows`.
#[cfg(windows)]
#[tokio::test]
async fn a_run_that_cannot_be_removed_reports_the_failure() {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    crate::runstate::with_isolated_runs_dir_async("runs-delete-locked", |_d| async move {
        create_run(&finished("run-stuck", 1)).unwrap();
        let held = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0)
            .open(crate::runstate::run_dir("run-stuck").join("held.bin"))
            .unwrap();

        let outcome = delete_run(AxumPath("run-stuck".to_string()), force(false)).await;
        drop(held);

        let (code, body) = outcome.expect_err("the removal fails");
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.0.error.contains("Failed to delete"));
    })
    .await;
}
