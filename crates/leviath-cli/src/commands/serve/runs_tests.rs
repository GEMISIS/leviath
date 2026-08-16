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
    for optional in ["read_paths", "final_output", "output_request"] {
        let r = resolve_ok(&[("fields", optional)]);
        let fields = r.fields.expect("fields set");
        assert!(fields.contains(optional), "{optional} should be selectable");
    }
}

/// A silently ignored parameter is the API smell that produces the worst bug
/// reports, so each conflicting combination is refused by name.
#[test]
fn ids_cannot_be_combined_with_the_parameters_it_would_override() {
    for conflicting in ["cursor", "q", "status", "since"] {
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
                name: "review".to_string(),
                index: 0,
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: None,
                ended_at: None,
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
                name: "work".to_string(),
                index: 0,
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: None,
                ended_at: None,
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
                name: "work".to_string(),
                index: 0,
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: None,
                ended_at: None,
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
                    }],
                    description: None,
                }],
            },
        )
        .unwrap();
        crate::runstate::write_stages_index(
            "run-deepquiet",
            &[leviath_core::run_meta::StageRecord {
                name: "work".to_string(),
                index: 0,
                status: leviath_core::run_meta::StageRunStatus::Complete,
                entered: true,
                prompt_tokens: 0,
                completion_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                region_tokens: Default::default(),
                first_call_prompt_tokens: None,
                runaway_warned: false,
                started_at: None,
                ended_at: None,
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
