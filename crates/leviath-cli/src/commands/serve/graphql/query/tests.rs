//! Tests for the read side of the schema.
//!
//! Whole queries run against the schema over an isolated runs directory, so
//! what is asserted is the answer a client gets rather than the shape of an
//! intermediate. The filters each have their own tests beside the input they
//! belong to.

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema, Variables};

use super::Query;
use crate::commands::serve::core::runs::{self as run_core, ParentFilter, SortKey, Source};
use crate::runstate::{RunMeta, create_run};

/// A run on disk, started at a known second so ordering is assertable.
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

/// A run of one blueprint, for the filter tests.
fn named(id: &str, blueprint: &str) -> RunMeta {
    let mut meta = meta_at(id, 100);
    meta.agent_name = blueprint.to_string();
    meta
}

/// Run one query against a schema wired to a daemon-less state.
async fn run_query(query: &str) -> async_graphql::Response {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

/// Run one query against a schema wired to the daemon `control` speaks to.
async fn run_query_with_daemon(
    control: leviath_runtime::control_socket::ControlClient,
    query: &str,
) -> async_graphql::Response {
    let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    state.control = control;
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

/// A filter nested past `MAX_FILTER_DEPTH`, written as `and` inside `and`.
///
/// Every surface that reads a run filter renders it to take a digest, and the
/// render is what refuses one too deep to walk. This is the shape that does
/// it, in the one place that spells it out.
fn too_deep(leaf: &str) -> String {
    let mut written = leaf.to_string();
    for _ in 0..super::super::paging::digest::MAX_FILTER_DEPTH {
        written = format!("{{ and: [{written}] }}");
    }
    written
}

/// Run one query with a `$path` variable.
///
/// A path travels as a variable rather than inside the query text: a Windows
/// path is full of backslashes, a backslash escapes inside a GraphQL string, and
/// interpolating one is a parse error on that platform and nowhere else.
async fn run_query_for_path(query: &str, path: &str) -> async_graphql::Response {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    schema
        .execute(
            Request::new(query)
                .variables(Variables::from_json(serde_json::json!({ "path": path }))),
        )
        .await
}

/// What `GET /api/runs` asks for with no query parameters at all.
///
/// Written out rather than reached for through the route, because the point of
/// the test below is that these exact defaults digest the way the GraphQL
/// listing's do.
fn rest_selection() -> run_core::RunSelection {
    run_core::RunSelection {
        limit: 2,
        blueprint: None,
        statuses: Vec::new(),
        sort: SortKey::Started,
        descending: true,
        order: None,
        q: None,
        sources: vec![Source::Meta, Source::Files],
        sources_raw: "meta,files".to_string(),
        fields: None,
        ids: None,
        since: None,
        parent: ParentFilter::Any,
        predicate: None,
        preloaded: None,
    }
}

/// The ids a `runs` answer carries, in the order they came back.
fn ids_of(data: &async_graphql::Value, field: &str) -> Vec<String> {
    let json = serde_json::to_value(data).expect("data serializes");
    json[field]["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|run| run["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A blueprint reference the schema cannot read is refused before the query
/// runs at all.
///
/// A reference says which installed blueprint, and manifest text is not part
/// of saying that: `validateBlueprint` takes the text as its own argument, so a
/// reference carrying text, and one carrying nothing, are both refused before
/// the field runs.
#[tokio::test]
async fn a_blueprint_reference_the_schema_cannot_read_is_refused() {
    for query in [
        r#"{ validateBlueprint(manifest: "[agent]", as: { content: "[agent]" }) { valid } }"#,
        r#"{ validateBlueprint(manifest: "[agent]", as: {}) { valid } }"#,
    ] {
        let answer = run_query(query).await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("name"), "{query}: {}", error.message);
    }
}

/// A stale pin is refused wherever a blueprint reference is read, before a
/// single directory is walked.
///
/// The pin is checked by the reference itself, so one blueprint installed under
/// a known digest answers for every field that takes one.
#[tokio::test]
async fn a_stale_blueprint_pin_is_refused_wherever_a_reference_is_read() {
    let agents = tempfile::tempdir().expect("a temp agents dir");
    let dir = agents.path().join("drifted");
    std::fs::create_dir_all(&dir).expect("the agent directory");
    std::fs::write(
        dir.join(leviath_core::files::MANIFEST_FILENAME),
        manifest_text("drifted", "1.0.0"),
    )
    .expect("the manifest is written");
    let stale = "0".repeat(64);

    crate::commands::serve::blueprints::TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async move {
            for query in [format!(
                r#"{{ validateBlueprint(manifest: "[agent]",
                         as: {{ name: "drifted", digest: "{stale}" }}) {{ valid }} }}"#
            )] {
                let answer = run_query(&query).await;
                let error = answer.errors.first().expect("a refusal");
                assert_eq!(
                    error
                        .extensions
                        .as_ref()
                        .and_then(|e| e.get("code"))
                        .map(ToString::to_string),
                    Some("\"CONFLICT\"".to_string()),
                    "{query}: {}",
                    error.message
                );
            }
        })
        .await;
}

/// A check needs the text to check, and takes nothing that points elsewhere.
#[tokio::test]
async fn validating_a_blueprint_with_no_text_is_refused() {
    for query in [
        r#"query { validateBlueprint(as: { name: "coder" }) { valid } }"#,
        r#"query { validateBlueprint(manifest: "[agent]", digest: "abc") { valid } }"#,
    ] {
        let answer = run_query(query).await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("validateBlueprint"),
            "{query}: {}",
            error.message
        );
    }
}

// ─── the listing ────────────────────────────────────────────────────────────

/// The whole round trip: runs on disk, a query naming the fields it wants, and
/// an answer carrying those fields and nothing else.
#[tokio::test]
async fn a_query_reads_runs_newest_first_and_pages() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-page", |_d| async move {
        for i in 0..5 {
            create_run(&meta_at(&format!("run-{i}"), 100 + i)).expect("run written");
        }

        let answer = run_query(
            r#"{ runs(first: 2) {
                    results { id blueprintName status task }
                    cursor
                    total
                    highlights { runId field }
                }
                serverTime }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        assert_eq!(ids_of(&answer.data, "runs"), vec!["run-4", "run-3"]);

        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 5);
        assert!(json["runs"]["cursor"].is_string(), "another page follows");
        assert_eq!(json["runs"]["highlights"].as_array().map(Vec::len), Some(0));
        assert!(json["serverTime"].as_i64().unwrap_or_default() > 0);
        assert_eq!(json["runs"]["results"][0]["status"], "STARTING");
        assert_eq!(json["runs"]["results"][0]["blueprintName"], "test-agent");
    })
    .await;
}

/// The cursor from one page starts the next, and the two do not overlap.
#[tokio::test]
async fn a_cursor_resumes_the_walk_where_it_stopped() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-cursor", |_d| async move {
        for i in 0..4 {
            create_run(&meta_at(&format!("run-{i}"), 100 + i)).expect("run written");
        }

        let first = run_query("{ runs(first: 2) { cursor } }").await;
        let json = serde_json::to_value(&first.data).expect("data serializes");
        let cursor = json["runs"]["cursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();
        let next = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { runs(first: 2, after: $after) { results { id } } }",
                )
                .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(next.errors.is_empty(), "{:?}", next.errors);
        assert_eq!(ids_of(&next.data, "runs"), vec!["run-1", "run-0"]);
    })
    .await;
}

/// An id that names nothing is an empty page rather than a refusal: one dead
/// id in a batch must not cost a client the rest of the batch.
#[tokio::test]
async fn a_batch_of_ids_answers_for_the_ones_that_are_there() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-missing", |_d| async move {
        create_run(&meta_at("run-real", 100)).expect("run written");

        let answer = run_query(
            r#"{ runs(filter: { id: { in: ["run-real", "run-ghost"] } }) {
                    results { id }
                    total
                } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        assert_eq!(ids_of(&answer.data, "runs"), vec!["run-real"]);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 1);
    })
    .await;
}

/// One run by id, and nothing for an id nothing answers to.
#[tokio::test]
async fn one_run_answers_to_its_id() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-by-id", |_d| async move {
        create_run(&meta_at("run-real", 100)).expect("run written");

        let found = run_query(r#"{ run(id: "run-real") { id blueprintName } }"#).await;
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let json = serde_json::to_value(&found.data).expect("data serializes");
        assert_eq!(json["run"]["id"], "run-real");

        let missing = run_query(r#"{ run(id: "run-ghost") { id } }"#).await;
        assert!(missing.errors.is_empty(), "{:?}", missing.errors);
        let json = serde_json::to_value(&missing.data).expect("data serializes");
        assert!(json["run"].is_null(), "not here is null, not a refusal");
    })
    .await;
}

/// A filter reaches the listing rather than being accepted and ignored, and a
/// cursor keeps paging correct underneath it.
///
/// The walk is taken a page at a time under a predicate that keeps three of
/// five runs, so a cursor that skipped or repeated one would show up as a
/// wrong page rather than as a wrong count.
#[tokio::test]
async fn a_filter_reaches_the_listing_and_pages_under_it() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-filtered", |_d| async move {
        for i in 0..5 {
            let mut run = meta_at(&format!("run-{i}"), 100 + i);
            run.status = match i % 2 {
                0 => leviath_core::run_meta::RunStatus::Error,
                _ => leviath_core::run_meta::RunStatus::Complete,
            };
            run.title = Some(format!("run number {i}"));
            create_run(&run).expect("run written");
        }

        let first = run_query(
            r#"{ runs(first: 2, filter: { status: { eq: ERROR } }) {
                    results { id } cursor total
                } }"#,
        )
        .await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(ids_of(&first.data, "runs"), vec!["run-4", "run-2"]);
        let json = serde_json::to_value(&first.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 3, "the count describes the filter");
        let cursor = json["runs"]["cursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();
        let next = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { runs(first: 2, after: $after,
                       filter: { status: { eq: ERROR } }) { results { id } } }",
                )
                .variables(Variables::from_json(
                    serde_json::json!({ "after": cursor.clone() }),
                )),
            )
            .await;
        assert!(next.errors.is_empty(), "{:?}", next.errors);
        assert_eq!(ids_of(&next.data, "runs"), vec!["run-0"]);

        // The same cursor against a different predicate names a walk that is
        // not the one being asked for, so it is refused rather than resumed.
        let crossed = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { runs(first: 2, after: $after,
                       filter: { status: { eq: COMPLETE } }) { total } }",
                )
                .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(
            crossed
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("different set of filters"),
            "{:?}",
            crossed.errors
        );

        // A combinator composes over the same listing.
        let either = run_query(
            r#"{ runs(filter: { or: [
                   { status: { eq: ERROR } },
                   { title: { endsWith: "number 1" } }
                 ] }) { results { id } total } }"#,
        )
        .await;
        assert!(either.errors.is_empty(), "{:?}", either.errors);
        let json = serde_json::to_value(&either.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 4);

        // And `not` around it selects exactly the rest.
        let rest = run_query(
            r#"{ runs(filter: { not: { or: [
                   { status: { eq: ERROR } },
                   { title: { endsWith: "number 1" } }
                 ] } }) { results { id } } }"#,
        )
        .await;
        assert_eq!(ids_of(&rest.data, "runs"), vec!["run-3"]);
    })
    .await;
}

/// A run with a stage ledger, which is the file a `stages` filter has to read.
fn with_ledger(id: &str, started_at: i64) {
    create_run(&meta_at(id, started_at)).expect("run written");
    crate::runstate::write_stages_index(
        id,
        &[leviath_core::run_meta::StageRecord::new(
            "build".to_string(),
            0,
        )],
    )
    .expect("the ledger");
}

/// Every run that has opened one of its own files to answer a field, in the
/// order they did.
static FILE_READS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Where the read log stands before a query runs.
///
/// Installing the recorder here rather than in one test is what lets any of
/// them take a mark: the first call wins and the rest are no-ops.
fn read_mark() -> usize {
    crate::commands::serve::graphql::types::run::record_file_reads(Box::new(|run_id| {
        leviath_core::sync::lock(&FILE_READS).push(run_id.to_string());
    }));
    leviath_core::sync::lock(&FILE_READS).len()
}

/// The runs that opened one of their own files since `mark`, sorted.
fn reads_since(mark: usize) -> Vec<String> {
    let mut read = leviath_core::sync::lock(&FILE_READS)[mark..].to_vec();
    read.sort();
    read.dedup();
    read
}

/// A filter that has to read a file opens nothing for a run the page it was
/// asked for does not reach, and nothing at all for one the cursor skipped.
///
/// This is the whole point of the lazy walk, and the only way to check it is to
/// count: the answers would be identical either way.
#[tokio::test]
async fn a_file_backed_page_reads_only_the_runs_it_reaches() {
    crate::runstate::with_isolated_runs_dir_async("graphql-lazy-reads", |_d| async move {
        for at in 0..6 {
            with_ledger(&format!("run-{at}"), 100 + at);
        }
        let query = r#"{ runs(first: 2, filter: { stages: { some: { name: { eq: "build" } } } })
                          { results { id } cursor } }"#;

        let mark = read_mark();
        let first = run_query(query).await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(ids_of(&first.data, "runs"), vec!["run-5", "run-4"]);
        // The page, and the one run past it that only decides whether there is
        // another page. Nothing older than that is opened.
        assert_eq!(
            reads_since(mark),
            vec![
                "run-3".to_string(),
                "run-4".to_string(),
                "run-5".to_string()
            ]
        );
        let json = serde_json::to_value(&first.data).expect("data serializes");
        let cursor = json["runs"]["cursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let mark = read_mark();
        let second = run_query(&format!(
            r#"{{ runs(first: 2, after: "{cursor}",
                       filter: {{ stages: {{ some: {{ name: {{ eq: "build" }} }} }} }})
                  {{ results {{ id }} }} }}"#
        ))
        .await;
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert_eq!(ids_of(&second.data, "runs"), vec!["run-3", "run-2"]);
        let read = reads_since(mark);
        assert!(
            !read.contains(&"run-5".to_string()) && !read.contains(&"run-4".to_string()),
            "page two opened a file for a run page one already passed: {read:?}"
        );
    })
    .await;
}

/// Every sort key the run listing offers runs the listing, and a run with no
/// title still has a place in the order.
///
/// The three timestamps are `GET /api/runs`'s own keys, so a cursor minted here
/// names the same walk there; the title is this listing's own, and it is the
/// one key a run can be missing.
#[tokio::test]
async fn every_run_sort_key_orders_the_listing() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-order", |_d| async move {
        let mut alpha = meta_at("alpha", 100);
        alpha.title = Some("a title".to_string());
        alpha.updated_at = 900;
        alpha.last_progress_at = Some(500);
        create_run(&alpha).expect("run written");

        let mut beta = meta_at("beta", 200);
        beta.title = Some("b title".to_string());
        beta.updated_at = 500;
        beta.last_progress_at = Some(900);
        create_run(&beta).expect("run written");

        // A run with no title of its own, which is where that key is absent.
        create_run(&meta_at("untitled", 300)).expect("run written");

        for (field, leader) in [
            ("STARTED_AT", "untitled"),
            ("UPDATED_AT", "alpha"),
            ("LAST_PROGRESS_AT", "beta"),
        ] {
            let answer = run_query(&format!(
                "{{ runs(orderBy: [{{ field: {field}, direction: DESC }}]) \
                   {{ results {{ id }} }} }}"
            ))
            .await;
            assert!(answer.errors.is_empty(), "{field}: {:?}", answer.errors);
            assert_eq!(
                ids_of(&answer.data, "runs").first().map(String::as_str),
                Some(leader),
                "{field} leads with the largest value"
            );
        }

        let titled =
            run_query("{ runs(orderBy: [{ field: TITLE, direction: ASC }]) { results { id } } }")
                .await;
        assert!(titled.errors.is_empty(), "{:?}", titled.errors);
        let ids = ids_of(&titled.data, "runs");
        assert_eq!(ids.len(), 3, "every run has a place: {ids:?}");
        let with_titles: Vec<String> = ids.iter().filter(|id| *id != "untitled").cloned().collect();
        assert_eq!(
            with_titles,
            vec!["alpha".to_string(), "beta".to_string()],
            "the titled runs run in title order"
        );
    })
    .await;
}

/// A filter answerable from the record opens nothing at all, count included.
#[tokio::test]
async fn a_record_only_filter_opens_no_file() {
    crate::runstate::with_isolated_runs_dir_async("graphql-lazy-cheap", |_d| async move {
        for at in 0..4 {
            with_ledger(&format!("run-{at}"), 100 + at);
        }
        let mark = read_mark();
        let answer = run_query(
            r#"{ runs(filter: { status: { in: [STARTING] } }) { results { id } total } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 4);
        assert!(
            reads_since(mark).is_empty(),
            "a cheap filter read a file: {:?}",
            reads_since(mark)
        );
    })
    .await;
}

/// `total` is a resolver, so a client that did not ask for it does not pay for
/// it; one that did asks for a pass over every page.
#[tokio::test]
async fn a_total_is_counted_only_where_it_is_selected() {
    crate::runstate::with_isolated_runs_dir_async("graphql-lazy-total", |_d| async move {
        for at in 0..6 {
            with_ledger(&format!("run-{at}"), 100 + at);
        }
        let filter = r#"filter: { stages: { some: { name: { eq: "build" } } } }"#;

        let mark = read_mark();
        let page = run_query(&format!(
            "{{ runs(first: 2, {filter}) {{ results {{ id }} }} }}"
        ))
        .await;
        assert!(page.errors.is_empty(), "{:?}", page.errors);
        assert_eq!(reads_since(mark).len(), 3, "the page and its look-ahead");

        let mark = read_mark();
        let counted = run_query(&format!("{{ runs(first: 2, {filter}) {{ total }} }}")).await;
        assert!(counted.errors.is_empty(), "{:?}", counted.errors);
        let json = serde_json::to_value(&counted.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 6, "every page, not what is left");
        assert_eq!(reads_since(mark).len(), 6, "the count settles the rest");
    })
    .await;
}

/// Every shortcut the old filter had is something the mirror says in full, and
/// each one reaches the listing.
#[tokio::test]
async fn the_mirror_replaces_every_run_filter_shortcut() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-shortcuts", |_d| async move {
        let mut parked = meta_at("parked", 100);
        parked.status = leviath_core::run_meta::RunStatus::WaitingInput;
        parked.waiting_on = Some(leviath_core::run_meta::WaitReason::UserPrompt);
        parked.agent_name = "asker".to_string();
        parked.stage_models = vec![leviath_core::run_meta::StageModelUse {
            provider: "anthropic".to_string(),
            model: "claude".to_string(),
        }];
        create_run(&parked).expect("run written");

        let mut done = meta_at("done", 200);
        done.status = leviath_core::run_meta::RunStatus::Complete;
        create_run(&done).expect("run written");

        for (query, wanted) in [
            (
                r#"{ runs(filter: { id: { in: ["parked"] } }) { results { id } } }"#,
                "parked",
            ),
            (
                r#"{ runs(filter: { status: { in: [WAITING_INPUT, PAUSED] } }) { results { id } } }"#,
                "parked",
            ),
            (
                r#"{ runs(filter: { waitReason: { reason: { eq: USER_PROMPT } } }) { results { id } } }"#,
                "parked",
            ),
            (
                r#"{ runs(filter: { stageModels: { some: { provider: { eq: "anthropic" } } } })
                     { results { id } } }"#,
                "parked",
            ),
            (
                r#"{ runs(filter: { blueprintName: { eq: "asker" } }) { results { id } } }"#,
                "parked",
            ),
            (
                r#"{ runs(filter: { status: { eq: COMPLETE } }) { results { id } } }"#,
                "done",
            ),
        ] {
            let answer = run_query(query).await;
            assert!(answer.errors.is_empty(), "{query}: {:?}", answer.errors);
            assert_eq!(ids_of(&answer.data, "runs"), vec![wanted.to_string()], "{query}");
        }
    })
    .await;
}

/// A filter reaches through a relation that costs a file, and through one that
/// does not, in the same request.
#[tokio::test]
async fn a_nested_filter_reaches_a_relation_that_reads() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-nested-io", |_d| async move {
        with_ledger("ledgered", 100);
        create_run(&meta_at("bare", 200)).expect("run written");

        let by_stage = run_query(
            r#"{ runs(filter: { stages: { some: { status: { eq: PENDING } } } })
                 { results { id } total } }"#,
        )
        .await;
        assert!(by_stage.errors.is_empty(), "{:?}", by_stage.errors);
        assert_eq!(ids_of(&by_stage.data, "runs"), vec!["ledgered".to_string()]);

        // The run that submitted nothing is the one `finalOutput: { isNull: true }`
        // selects, and both runs here have submitted nothing.
        let unanswered =
            run_query(r#"{ runs(filter: { finalOutput: { isNull: true } }) { total } }"#).await;
        assert!(unanswered.errors.is_empty(), "{:?}", unanswered.errors);
        let json = serde_json::to_value(&unanswered.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 2);
    })
    .await;
}

/// The unfiltered cursor is one token both surfaces mint and both surfaces
/// read, which is what lets a client move a walk between them.
#[tokio::test]
async fn an_unfiltered_cursor_crosses_between_rest_and_graphql() {
    crate::runstate::with_isolated_runs_dir_async("graphql-rest-cursor", |dir| async move {
        for at in 0..4 {
            create_run(&meta_at(&format!("run-{at}"), 100 + at)).expect("run written");
        }
        let state =
            crate::commands::serve::testutil::state_with_agent_paths(vec![dir.join("agents")]);

        // What `GET /api/runs` mints resumes the GraphQL walk.
        let spec = rest_selection().resolve(None).expect("a spec");
        let minted = run_core::list(&state, &spec)
            .await
            .next_cursor
            .expect("another page follows");
        let resumed = run_query(&format!(
            r#"{{ runs(first: 2, after: "{minted}") {{ results {{ id }} }} }}"#
        ))
        .await;
        assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
        assert_eq!(ids_of(&resumed.data, "runs"), vec!["run-1", "run-0"]);

        // And what the GraphQL walk mints resumes the REST listing.
        let page = run_query("{ runs(first: 2) { cursor } }").await;
        let json = serde_json::to_value(&page.data).expect("data serializes");
        let cursor = json["runs"]["cursor"]
            .as_str()
            .expect("a cursor")
            .to_string();
        let spec = rest_selection()
            .resolve(Some(&cursor))
            .expect("the REST listing takes it");
        let rest = run_core::list(&state, &spec).await;
        assert_eq!(
            rest.hits
                .iter()
                .map(|hit| hit.meta.run_id.clone())
                .collect::<Vec<_>>(),
            vec!["run-1".to_string(), "run-0".to_string()]
        );
    })
    .await;
}

/// The search joins the cursor's digest, so a cursor cannot be carried from
/// one search to another.
#[tokio::test]
async fn a_cursor_minted_under_one_search_is_refused_by_another() {
    crate::runstate::with_isolated_runs_dir_async("graphql-search-cursor", |_d| async move {
        for at in 0..4 {
            let mut run = meta_at(&format!("run-{at}"), 100 + at);
            run.task = "find the parser bug".to_string();
            create_run(&run).expect("run written");
        }

        let page = run_query(r#"{ runs(first: 2, search: { query: "parser" }) { cursor } }"#).await;
        assert!(page.errors.is_empty(), "{:?}", page.errors);
        let json = serde_json::to_value(&page.data).expect("data serializes");
        let cursor = json["runs"]["cursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let crossed = run_query(&format!(
            r#"{{ runs(first: 2, after: "{cursor}", search: {{ query: "bug" }}) {{ total }} }}"#
        ))
        .await;
        assert!(
            crossed
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("different set of filters"),
            "{:?}",
            crossed.errors
        );
    })
    .await;
}

/// A batch fetch by id is still filtered, and the ids it cannot find are still
/// reported.
#[tokio::test]
async fn a_batch_fetch_composes_with_the_rest_of_the_filter() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-ids-filter", |_d| async move {
        let mut failed = meta_at("run-failed", 100);
        failed.status = leviath_core::run_meta::RunStatus::Error;
        create_run(&failed).expect("run written");
        create_run(&meta_at("run-fine", 200)).expect("run written");

        let answer = run_query(
            r#"{ runs(filter: { id: { in: ["run-failed", "run-fine", "run-ghost"] },
                                status: { eq: ERROR } }) {
                    results { id } total
                } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        assert_eq!(ids_of(&answer.data, "runs"), vec!["run-failed"]);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 1, "a run that was read but dropped");
    })
    .await;
}

/// A subtree named inside a batch fetch is resolved from the index, which is
/// the one thing a run's own record cannot answer.
#[tokio::test]
async fn a_batch_fetch_can_ask_about_a_subtree() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-ids-subtree", |_d| async move {
        create_run(&meta_at("root", 100)).expect("run written");
        let mut worker = meta_at("worker", 200);
        worker.parent_run_id = Some("root".to_string());
        create_run(&worker).expect("run written");
        let mut grandchild = meta_at("grandchild", 300);
        grandchild.parent_run_id = Some("worker".to_string());
        create_run(&grandchild).expect("run written");

        let answer = run_query(
            r#"{ runs(filter: { id: { in: ["root", "worker", "grandchild"] },
                                ancestorIds: { has: "root" } }) {
                    results { id } total
                } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let mut ids = ids_of(&answer.data, "runs");
        ids.sort();
        assert_eq!(ids, vec!["grandchild".to_string(), "worker".to_string()]);
    })
    .await;
}

/// A page of runs each naming its ancestors links the store once, not once per
/// run.
///
/// `ancestorIds` is answered by walking a run's `parentId` up through the
/// shared index, and a chain is three deep at most. Linking the whole store per
/// run would make a page of two hundred cost the store two hundred times over,
/// and the answer would look exactly the same - so what this checks is the work
/// that did not happen.
#[tokio::test]
async fn a_page_of_ancestors_does_not_link_the_store_once_per_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-ancestors-cost", |_d| async move {
        // Newest first, so the ids below read as the breadcrumb they are.
        create_run(&meta_at("anc7-root", 100)).expect("run written");
        let mut mid = meta_at("anc7-mid", 200);
        mid.parent_run_id = Some("anc7-root".to_string());
        create_run(&mid).expect("run written");
        for at in 0..198 {
            let mut leaf = meta_at(&format!("anc7-leaf-{at:03}"), 300 + at);
            leaf.parent_run_id = Some("anc7-mid".to_string());
            create_run(&leaf).expect("run written");
        }

        let before = crate::commands::serve::testutil::trees_built_over("anc7-");
        let answer = run_query("{ runs(first: 200) { results { id ancestorIds } } }").await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let built = crate::commands::serve::testutil::trees_built_over("anc7-") - before;
        assert!(
            built <= 1,
            "the store was linked {built} times for one page"
        );

        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let results = json["runs"]["results"].as_array().expect("a page");
        assert_eq!(results.len(), 200);
        let chain_of = |id: &str| {
            results
                .iter()
                .find(|row| row["id"] == id)
                .map(|row| row["ancestorIds"].clone())
                .expect("the run is on the page")
        };
        // Root first, so the list reads as a breadcrumb.
        assert_eq!(
            chain_of("anc7-leaf-000"),
            serde_json::json!(["anc7-root", "anc7-mid"])
        );
        assert_eq!(chain_of("anc7-mid"), serde_json::json!(["anc7-root"]));
        assert_eq!(chain_of("anc7-root"), serde_json::json!([]));
    })
    .await;
}

/// A record that names one of its own descendants as its parent stops the walk
/// rather than spinning it.
///
/// Nothing writes such a record. The guard is that a run already on the chain
/// is not walked to twice.
#[tokio::test]
async fn a_cycle_above_a_run_ends_the_breadcrumb() {
    crate::runstate::with_isolated_runs_dir_async("graphql-ancestors-cycle", |_d| async move {
        let mut one = meta_at("cyc-one", 100);
        one.parent_run_id = Some("cyc-two".to_string());
        create_run(&one).expect("run written");
        let mut two = meta_at("cyc-two", 200);
        two.parent_run_id = Some("cyc-one".to_string());
        create_run(&two).expect("run written");

        let answer = run_query(
            r#"{ runs(filter: { id: { eq: "cyc-one" } }) { results { id ancestorIds } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(
            json["runs"]["results"][0]["ancestorIds"],
            serde_json::json!(["cyc-one", "cyc-two"]),
            "each run above is named once and the walk stops"
        );
    })
    .await;
}

/// A refused request names what was wrong and carries the code a client
/// branches on, rather than an uncoded message it would have to read.
#[tokio::test]
async fn a_refused_request_carries_its_code() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-refused", |_d| async move {
        let answer = run_query("{ runs(first: 100000) { total } }").await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("run page cap"), "{}", error.message);
        let extensions = error.extensions.as_ref().expect("extensions");
        assert_eq!(
            extensions.get("code").map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );
    })
    .await;
}

/// Only the runs a parent filter names come back, which is the paged answer to
/// a fan-out that `children` returns unbounded.
#[tokio::test]
async fn a_parent_filter_pages_one_runs_children() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-parent", |_d| async move {
        create_run(&meta_at("root", 100)).expect("run written");
        for i in 0..3 {
            let mut child = meta_at(&format!("worker-{i}"), 200 + i);
            child.parent_run_id = Some("root".to_string());
            create_run(&child).expect("run written");
        }

        let children = run_query(
            r#"{ runs(filter: { parentId: { eq: "root" } }) { results { id parentId } total } }"#,
        )
        .await;
        assert!(children.errors.is_empty(), "{:?}", children.errors);
        assert_eq!(
            ids_of(&children.data, "runs"),
            vec!["worker-2", "worker-1", "worker-0"]
        );

        let roots =
            run_query("{ runs(filter: { parentId: { isNull: true } }) { results { id } } }").await;
        assert_eq!(ids_of(&roots.data, "runs"), vec!["root"]);
    })
    .await;
}

/// A run answers for the blueprint it executed, from its own snapshot.
///
/// The installed file is edited in between, and the run still answers with what
/// it ran: that is the whole reason the snapshot exists.
#[tokio::test]
async fn a_run_answers_with_the_blueprint_it_executed() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-blueprint", |_d| async move {
        let installed = tempfile::tempdir().expect("a temp dir");
        let path = installed.path().join("agent.leviath");
        std::fs::write(&path, "[agent]\nname = \"coder\"\nversion = \"9.9.9\"\n")
            .expect("installed written");

        let mut meta = meta_at("coder-1788924523-abc123", 100);
        meta.agent_path = path.to_string_lossy().into_owned();
        let ran = "[agent]\nname = \"coder\"\nversion = \"1.0.0\"\n";
        meta.blueprint_digest = Some(crate::commands::serve::core::blueprints::digest_of(ran));
        create_run(&meta).expect("run written");
        std::fs::write(
            crate::commands::serve::core::blueprints::run_dir(&meta.run_id)
                .join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
            ran,
        )
        .expect("snapshot written");

        let answer = run_query(
            "{ runs { results { blueprintDigest blueprint { name version source digest } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["results"][0];
        assert_eq!(
            node["blueprint"]["version"], "1.0.0",
            "what ran, not what is installed"
        );
        assert_eq!(node["blueprint"]["source"], "SNAPSHOT");
        assert_eq!(node["blueprint"]["digest"], node["blueprintDigest"]);
    })
    .await;
}

/// A run from before snapshots existed falls back to the installed blueprint,
/// and says so. Its digest is null, because what it executed is unknown.
#[tokio::test]
async fn a_run_without_a_snapshot_reads_the_installed_blueprint() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-installed", |_d| async move {
        let installed = tempfile::tempdir().expect("a temp dir");
        let path = installed.path().join("agent.leviath");
        std::fs::write(&path, "[agent]\nname = \"coder\"\nversion = \"9.9.9\"\n")
            .expect("installed written");
        let mut meta = meta_at("coder-1788924523-old000", 100);
        meta.agent_path = path.to_string_lossy().into_owned();
        create_run(&meta).expect("run written");

        let answer =
            run_query("{ runs { results { blueprintDigest blueprint { version source } } } }")
                .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["results"][0];
        assert_eq!(node["blueprint"]["version"], "9.9.9");
        assert_eq!(node["blueprint"]["source"], "INSTALLED");
        assert!(node["blueprintDigest"].is_null(), "unknown, not the same");
    })
    .await;
}

/// A run whose blueprint is gone nulls that one field and says why, leaving the
/// rest of the page intact. One unreadable file must not cost a client the
/// forty-nine runs beside it.
#[tokio::test]
async fn an_unreadable_blueprint_nulls_one_field_and_keeps_the_page() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-noblueprint", |_d| async move {
        let mut gone = meta_at("coder-1788924523-gone00", 200);
        gone.agent_path = "/nowhere/agent.leviath".to_string();
        create_run(&gone).expect("run written");
        create_run(&meta_at("coder-1788924523-fine00", 100)).expect("run written");

        let answer = run_query("{ runs { results { id blueprint { name } } } }").await;
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let results = json["runs"]["results"].as_array().expect("results");
        assert_eq!(results.len(), 2, "both runs are still on the page");
        assert!(results[0]["blueprint"].is_null(), "the field is null");
        assert_eq!(results[0]["id"], "coder-1788924523-gone00");
        // The runs resolve side by side, so which unreadable blueprint is
        // reported first is not fixed. Both are named, and each carries the
        // code a client branches on.
        assert!(
            answer.errors.iter().all(|error| error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string)
                == Some("\"NOT_FOUND\"".to_string())),
            "{:?}",
            answer.errors
        );
        assert!(
            answer
                .errors
                .iter()
                .any(|error| error.message.contains("agent.leviath")),
            "{:?}",
            answer.errors
        );
    })
    .await;
}

/// A catalogue of blueprints, each with whatever the manifest declares.
///
/// Written to a temp directory and pointed at with `state_with_agent_paths`,
/// never with an empty list: that reads the real `~/.leviath/agents` and makes
/// a test depend on whoever ran it.
async fn catalogue(manifests: &[(&str, &str)]) -> Schema<Query, EmptyMutation, EmptySubscription> {
    let agents = Box::leak(Box::new(tempfile::tempdir().expect("a temp dir")));
    for (name, body) in manifests {
        let dir = agents.path().join(name);
        std::fs::create_dir_all(&dir).expect("agent dir");
        std::fs::write(dir.join(leviath_core::files::MANIFEST_FILENAME), body)
            .expect("manifest written");
    }
    let state =
        crate::commands::serve::testutil::state_with_agent_paths(vec![agents.path().to_path_buf()]);
    Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish()
}

/// One manifest naming nothing but the agent.
fn plain(name: &str, version: &str) -> String {
    format!("[agent]\nname = \"{name}\"\nversion = \"{version}\"\n")
}

/// Run one query and refuse to read an answer that failed.
async fn answer(
    schema: &Schema<Query, EmptyMutation, EmptySubscription>,
    query: &str,
) -> serde_json::Value {
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// The installed blueprints, by name, with the digest that says which bytes
/// they are.
#[tokio::test]
async fn the_blueprint_listing_reads_what_is_installed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let beta = plain("beta", "2.0.0");
        let schema = catalogue(&[("alpha", &alpha), ("beta", &beta)]).await;

        let json = answer(
            &schema,
            "{ blueprints { results { name version source digest } cursor total } }",
        )
        .await;
        let listing = &json["blueprints"];
        assert_eq!(listing["total"], 2);
        assert_eq!(listing["results"][0]["name"], "alpha");
        assert_eq!(listing["results"][0]["version"], "1.0.0");
        // A listing is always the live definition, never a run's frozen copy.
        assert_eq!(listing["results"][0]["source"], "INSTALLED");
        assert_eq!(listing["results"][1]["name"], "beta");
        assert!(listing["cursor"].is_null(), "one page holds them both");
    })
    .await;
}

/// The shortcuts the old filter had are what the mirror says in full.
#[tokio::test]
async fn the_mirror_replaces_every_filter_shortcut() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let beta = plain("beta", "2.0.0");
        let schema = catalogue(&[("alpha", &alpha), ("beta", &beta)]).await;

        // `names: [..]` is `name: { in: [..] }`, and a name nothing is
        // installed under is simply not in the answer.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: { name: { in: ["alpha", "ghost"] } })
                   { results { name } total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 1);
        assert_eq!(json["blueprints"]["results"][0]["name"], "alpha");

        // `query: ".."` is `name: { startsWith: ".." }`.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: { name: { startsWith: "ghos" } }) { total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 0, "the prefix matches nothing");
    })
    .await;
}

/// The combinators compose, and `isNull` asks about a value that is absent.
#[tokio::test]
async fn the_mirror_composes_and_asks_about_absence() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = "[agent]\nname = \"alpha\"\nversion = \"1.0.0\"\nentry_stage = \"plan\"\n\n                     [[stages]]\nname = \"plan\"\n";
        let beta = plain("beta", "2.0.0");
        let schema = catalogue(&[("alpha", alpha), ("beta", &beta)]).await;

        let json = answer(
            &schema,
            r#"{ blueprints(filter: {
                   or: [{ name: { eq: "alpha" } }, { name: { eq: "beta" } }]
                   not: { version: { eq: "2.0.0" } }
                   and: [{ name: { startsWith: "a" } }]
                 }) { results { name } total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 1);
        assert_eq!(json["blueprints"]["results"][0]["name"], "alpha");

        // Only the blueprint that named no entry stage has none.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: { entryStageName: { isNull: true } }) { results { name } } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["results"][0]["name"], "beta");
        assert_eq!(
            json["blueprints"]["results"].as_array().map(Vec::len),
            Some(1)
        );
    })
    .await;
}

/// A filter reaches through a relation: the regions a blueprint declares.
#[tokio::test]
async fn a_nested_filter_reaches_through_a_list_relation() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let pinned = "[agent]\nname = \"pinned\"\n\n\
                      [context.regions.brief]\nkind = \"pinned\"\nmax_tokens = 100\n";
        let sliding = "[agent]\nname = \"sliding\"\n\n\
                       [context.regions.log]\nkind = \"sliding_window\"\nmax_tokens = 100\n";
        let schema = catalogue(&[("pinned", pinned), ("sliding", sliding)]).await;

        let json = answer(
            &schema,
            r#"{ blueprints(filter: { regions: { some: { kind: { eq: PINNED } } } })
                   { results { name regions { name kind } } total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 1);
        assert_eq!(json["blueprints"]["results"][0]["name"], "pinned");
        assert_eq!(
            json["blueprints"]["results"][0]["regions"][0]["kind"],
            "PINNED"
        );

        // `none` is the other way round, and it keeps the other one.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: { regions: { none: { kind: { eq: PINNED } } } })
                   { results { name } } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["results"][0]["name"], "sliding");
    })
    .await;
}

/// `orderBy` runs the catalogue either way, and omitting it runs it by name.
#[tokio::test]
async fn the_listing_runs_in_the_order_it_was_asked_for() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let beta = plain("beta", "2.0.0");
        let schema = catalogue(&[("alpha", &alpha), ("beta", &beta)]).await;

        let json = answer(&schema, "{ blueprints { results { name } } }").await;
        assert_eq!(json["blueprints"]["results"][0]["name"], "alpha");

        let json = answer(
            &schema,
            "{ blueprints(orderBy: [{ field: NAME, direction: DESC }]) { results { name } } }",
        )
        .await;
        assert_eq!(json["blueprints"]["results"][0]["name"], "beta");

        let json = answer(
            &schema,
            "{ blueprints(orderBy: [{ field: VERSION, direction: ASC }]) { results { version } } }",
        )
        .await;
        assert_eq!(json["blueprints"]["results"][0]["version"], "1.0.0");
    })
    .await;
}

/// Paging the listing: a page, then the rest, resumed from the cursor the
/// first page handed back.
///
/// The cursor is a keyset on the name rather than an offset, so a blueprint
/// installed or removed between the two requests cannot make the second page
/// skip or repeat one.
#[tokio::test]
async fn the_blueprint_listing_pages() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let manifests: Vec<(&str, String)> = ["alpha", "beta", "gamma"]
            .into_iter()
            .map(|name| (name, plain(name, "1.0.0")))
            .collect();
        let borrowed: Vec<(&str, &str)> = manifests
            .iter()
            .map(|(name, body)| (*name, body.as_str()))
            .collect();
        let schema = catalogue(&borrowed).await;

        let json = answer(
            &schema,
            "{ blueprints(first: 2) { results { name } cursor } }",
        )
        .await;
        assert_eq!(
            json["blueprints"]["results"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(json["blueprints"]["results"][0]["name"], "alpha");
        let cursor = json["blueprints"]["cursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let rest = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { blueprints(first: 2, after: $after) {
                       results { name } cursor } }",
                )
                .variables(Variables::from_json(
                    serde_json::json!({ "after": cursor.clone() }),
                )),
            )
            .await;
        assert!(rest.errors.is_empty(), "{:?}", rest.errors);
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        assert_eq!(json["blueprints"]["results"][0]["name"], "gamma");
        assert!(
            json["blueprints"]["cursor"].is_null(),
            "no cursor is minted for a page nothing follows"
        );

        // A cursor minted for one filter cannot resume another: the walk it
        // names is not the walk being asked for.
        let crossed = schema
            .execute(
                Request::new(
                    r#"query($after: Cursor) { blueprints(first: 2, after: $after,
                         filter: { name: { startsWith: "a" } }) { total } }"#,
                )
                .variables(Variables::from_json(
                    serde_json::json!({ "after": cursor.clone() }),
                )),
            )
            .await;
        assert!(
            crossed
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("different set of filters"),
            "{:?}",
            crossed.errors
        );

        // Nor another order, for the same reason.
        let reordered = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { blueprints(first: 2, after: $after,
                       orderBy: [{ field: NAME, direction: DESC }]) { total } }",
                )
                .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(
            !reordered.errors.is_empty(),
            "a cursor is bound to its order"
        );

        let mangled = schema
            .execute(
                Request::new("query($after: Cursor) { blueprints(after: $after) { total } }")
                    .variables(Variables::from_json(serde_json::json!({ "after": "zzz" }))),
            )
            .await;
        assert!(
            mangled
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("Invalid cursor"),
            "{:?}",
            mangled.errors
        );
    })
    .await;
}

/// A page larger than the cap is refused rather than clamped.
#[tokio::test]
async fn the_blueprint_listing_refuses_an_oversized_page() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let schema = catalogue(&[("alpha", &alpha)]).await;
        let refused = schema
            .execute(Request::new("{ blueprints(first: 5000) { total } }"))
            .await;
        assert!(
            refused
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("the blueprint page cap"),
            "{:?}",
            refused.errors
        );
    })
    .await;
}

/// A filter nested past the limit is refused before anything is matched.
#[tokio::test]
async fn a_filter_nested_too_deep_is_refused() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let schema = catalogue(&[("alpha", &alpha)]).await;
        let mut filter = String::from(r#"{ name: { eq: "alpha" } }"#);
        for _ in 0..20 {
            filter = format!("{{ not: {filter} }}");
        }
        let refused = schema
            .execute(Request::new(format!(
                "{{ blueprints(filter: {filter}) {{ total }} }}"
            )))
            .await;
        assert!(
            refused
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("levels deep"),
            "{:?}",
            refused.errors
        );
    })
    .await;
}

/// `total` costs nothing where it is not selected.
#[tokio::test]
async fn an_unselected_total_is_never_counted() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let beta = plain("beta", "2.0.0");
        let schema = catalogue(&[("alpha", &alpha), ("beta", &beta)]).await;
        let json = answer(&schema, "{ blueprints(first: 1) { results { name } } }").await;
        assert_eq!(
            json["blueprints"]["results"].as_array().map(Vec::len),
            Some(1)
        );
        assert!(
            json["blueprints"].get("total").is_none(),
            "a field nobody selected is not in the answer"
        );
        // And selecting it counts the whole listing, not the page.
        let json = answer(&schema, "{ blueprints(first: 1) { total } }").await;
        assert_eq!(json["blueprints"]["total"], 2);
    })
    .await;
}

/// One blueprint by name, and nothing for a name nothing is installed under.
#[tokio::test]
async fn one_blueprint_answers_to_its_name() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let schema = catalogue(&[("alpha", &alpha)]).await;
        let json = answer(&schema, r#"{ blueprint(name: "alpha") { name version } }"#).await;
        assert_eq!(json["blueprint"]["name"], "alpha");
        assert_eq!(json["blueprint"]["version"], "1.0.0");

        let json = answer(&schema, r#"{ blueprint(name: "ghost") { name } }"#).await;
        assert!(
            json["blueprint"].is_null(),
            "a name nothing is installed under answers null"
        );
    })
    .await;
}

/// Every input the blueprint mirror reaches is one a client can write.
///
/// Each request below names a field of every generated input under
/// `BlueprintInput`, so each one is parsed off a real request rather than
/// built in Rust. A mirror that registers but cannot be read is a filter a
/// client writes and the server refuses, and nothing else here would catch it.
///
/// Three requests rather than one because a filter has a size limit, and one
/// naming every field of every nested input is over it. That limit is doing
/// its job: nothing a person writes looks like this.
#[tokio::test]
async fn every_mirrored_input_is_one_a_client_can_write() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let alpha = plain("alpha", "1.0.0");
        let schema = catalogue(&[("alpha", &alpha)]).await;

        // What a blueprint is, at the top level.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: {
                   id: { ne: "nothing" }
                   name: { startsWith: "a" }
                   digest: { isNull: false }
                   source: { in: [INSTALLED] }
                   version: { ne: "0" }
                   description: { isNull: false }
                   entryStageName: { isNull: false }
                   maxChildDepth: { lt: 99 }
                   toolRescan: { ne: AT_SPAWN_ONLY }
                   toolGuidance: { batchIndependentCalls: { eq: INHERIT }
                                   shellForMultiStepWork: { ne: OMIT } }
                   readPaths: { isEmpty: false }
                   isNull: false
                 }) { total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 0);

        // What a region is, quantified over the list of them.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: {
                   regions: {
                     some: {
                       name: { ne: "" }
                       kind: { notIn: [PINNED] }
                       maxTokens: { gte: 0 }
                       budgetPercent: { lt: 1.0 }
                       minTokens: { isNull: true }
                       budgetMaxTokens: { isNull: true }
                       description: { isNull: true }
                       required: { eq: false }
                       requiredMessage: { isNull: true }
                       describeInPrompt: { ne: false }
                       summarizable: { eq: true }
                       volatility: { ne: STABLE }
                       admission: { in: [EVICT] }
                       compactAt: { isNull: true }
                       accepts: { has: "text/plain" }
                       maxItems: { isNull: true }
                       strategy: { isNull: true }
                       overflow: { isNull: true }
                       compactCount: { isNull: true }
                       thresholdTokens: { isNull: true }
                       sourceRegionName: { isNull: true }
                       maxEntries: { isNull: true }
                       script: { isNull: true }
                       pinned: { isNull: true }
                       sourceRegion: { isNull: true }
                     }
                     every: { name: { isNull: false } }
                     none: { name: { eq: "" } }
                     isNull: false
                   }
                 }) { total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 0);

        // What fills a region, one alternative per variant.
        let json = answer(
            &schema,
            r#"{ blueprints(filter: {
                   regions: { some: { seed: { or: [
                     { seedFromCaller: { key: { eq: "task" } } }
                     { seedFromGlob: { pattern: { eq: "*.rs" } } }
                     { seedFromFiles: { paths: { has: "README.md" } } }
                     { seedFromLiteral: { text: { isNull: false } } }
                     { seedFromScript: { script: { ne: "" } } }
                     { seedFromCommand: { command: { ne: "" } } }
                     { seedFromTools: {
                         refresh: { eq: ONCE }
                         calls: { some: { tool: { ne: "" } args: { isNull: false } }
                                  every: { tool: { isNull: false } }
                                  none: { tool: { eq: "" } } isNull: false }
                     } }
                   ] not: { seedFromCaller: { key: { eq: "" } } } isNull: false } } }
                 }) { total } }"#,
        )
        .await;
        assert_eq!(json["blueprints"]["total"], 0);
    })
    .await;
}

/// Who is on the other end of the control socket, with nothing on it.
///
/// A daemon-less server answers rather than failing, because this read is
/// exactly the one a client makes to find out that the daemon is down.
#[tokio::test]
async fn the_daemon_status_answers_with_no_daemon() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let json =
            run_query("{ daemon { reachable version build pid restarts restartAdvised toolEnv } }")
                .await;
        assert!(json.errors.is_empty(), "{:?}", json.errors);
        let data = serde_json::to_value(&json.data).expect("data serializes");
        let daemon = &data["daemon"];
        // Nothing has been asked of it, and silence is not evidence either way.
        assert_eq!(daemon["reachable"], true);
        assert!(daemon["version"].is_null(), "no daemon has said one");
        assert_eq!(daemon["restarts"], 0);
    })
    .await;
}

/// What an update would do, read without reaching the network.
///
/// Two states, because the config decides whether asking also starts a check
/// for whoever asks next, and both halves of that answer the same way here.
#[tokio::test]
async fn the_update_plan_is_read_without_reaching_the_network() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let json =
            run_query("{ updatePlan { version installMethod channel latest updateAvailable } }")
                .await;
        assert!(json.errors.is_empty(), "{:?}", json.errors);
        let data = serde_json::to_value(&json.data).expect("data serializes");
        assert_eq!(data["updatePlan"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            data["updatePlan"]["latest"].is_null(),
            "nothing has been checked yet"
        );

        // The same read with the check switched off, which is the other half
        // of the one decision this field makes.
        let config = home.join("config.toml");
        std::fs::write(
            &config,
            "update_check = false
",
        )
        .expect("a config");
        let state = crate::commands::serve::testutil::state_with_config_at(&config);
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();
        let answer = schema
            .execute(Request::new("{ updatePlan { version updateAvailable } }"))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let data = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(data["updatePlan"]["version"], env!("CARGO_PKG_VERSION"));
    })
    .await;
}

/// The catalogue fields answer from what this machine has configured.
///
/// A daemon-less state configures no provider, so the honest answer is empty
/// lists rather than an error: "nothing configured" is a state, not a failure.
#[tokio::test]
async fn the_catalogue_answers_for_an_unconfigured_machine() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            "{ models { results { id modelId providerId providerName } total }
               providers { results { id name display enabled signedIn } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["models"]["results"].as_array().map(Vec::len), Some(0));
        assert_eq!(json["models"]["total"], 0);
        // Every provider Leviath can sign in to is listed, configured or not,
        // which is what a settings screen needs to offer them.
        let providers = json["providers"]["results"].as_array().expect("providers");
        assert!(!providers.is_empty(), "the sign-in providers are listed");
        assert!(
            providers.iter().all(|p| p["enabled"] == false),
            "nothing is configured here: {providers:?}"
        );
    })
    .await;
}

/// The tool listing carries the built-ins and whatever could not be offered,
/// and the group tokens are a root field of their own.
#[tokio::test]
async fn the_tool_listing_carries_its_skips_and_the_group_tokens() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            "{ tools(first: 200) { results { name origin } skipped { path reason } }
               toolGroups { name description } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let tools = json["tools"]["results"].as_array().expect("tools");
        assert!(
            tools.iter().any(|t| t["name"] == "read_file"),
            "the built-ins are there"
        );
        let groups = json["toolGroups"].as_array().expect("groups");
        assert!(
            groups.iter().any(|g| g["name"] == "@builtin"),
            "the group tokens are named: {groups:?}"
        );
    })
    .await;
}

/// A script that was found and cannot be offered is reported, with the reason.
///
/// Silence here is the failure worth preventing: an author who believes a tool
/// exists, and whose agent is never offered it, has nothing to read.
#[tokio::test]
async fn a_script_that_cannot_be_offered_is_reported() {
    crate::commands::serve::testutil::with_home(|home| async move {
        // An agent whose own `tools/` holds a script that will not compile.
        let agent = home.join(".leviath").join("agents").join("coder");
        std::fs::create_dir_all(agent.join("tools")).expect("the agent's tools dir");
        std::fs::write(
            agent.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"coder\"\n",
        )
        .expect("manifest written");
        std::fs::write(
            agent.join("tools").join("broken.rhai"),
            "fn main( { this does not compile",
        )
        .expect("script written");

        let answer =
            run_query(r#"{ blueprint(name: "coder") { tools { skipped { path reason } } } }"#)
                .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let skipped = json["blueprint"]["tools"]["skipped"]
            .as_array()
            .expect("skipped");
        assert!(
            skipped.iter().any(|s| s["path"]
                .as_str()
                .unwrap_or_default()
                .ends_with("broken.rhai")),
            "the broken script is named: {skipped:?}"
        );
        assert!(
            skipped
                .iter()
                .all(|s| !s["reason"].as_str().unwrap_or_default().is_empty()),
            "each one says why: {skipped:?}"
        );
    })
    .await;
}

/// An agent name that could escape the agents directory is refused, on this
/// surface as on the REST one: the name arrives from a client and `join`
/// resists neither `..` nor an absolute path.
#[tokio::test]
async fn a_tool_scope_refuses_an_unsafe_agent_name() {
    crate::commands::serve::testutil::with_home(|home| async move {
        // A manifest declaring a name that would leave the agents directory.
        // The name is the manifest's, not the directory's, so this is a
        // blueprint the listing hands back and whose scope has to be refused.
        let dir = home.join(".leviath").join("agents").join("sneaky");
        std::fs::create_dir_all(&dir).expect("the agent directory");
        std::fs::write(
            dir.join(leviath_core::files::MANIFEST_FILENAME),
            manifest_text("../etc", "1.0.0"),
        )
        .expect("a manifest");

        let answer =
            run_query(r#"{ blueprint(name: "../etc") { tools { results { name } } } }"#).await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("Invalid agent name"),
            "{}",
            error.message
        );
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );
    })
    .await;
}

/// A run's answer, read from the run's own directory when a client asks for
/// it and not before.
#[tokio::test]
async fn a_run_carries_the_answer_it_submitted() {
    crate::runstate::with_isolated_runs_dir_async("graphql-final-output", |_d| async move {
        // The descriptor in `meta.json` says an answer exists; the bytes live
        // in the sidecar beside it, which is how the daemon stores it.
        let mut meta = meta_at("coder-1788924523-out000", 100);
        meta.final_output = Some(leviath_core::FinalOutputDescriptor {
            format: Some("markdown".to_string()),
            stage: "output".to_string(),
            submitted_at: 1_788_924_600,
            bytes: 10,
            truncated: false,
            artifacts: Vec::new(),
        });
        create_run(&meta).expect("run written");
        crate::runstate::write_final_output(
            &crate::commands::serve::core::blueprints::run_dir(&meta.run_id),
            "the answer",
        )
        .expect("output written");

        let answer = run_query(
            "{ runs { results { finalOutput { content format stage submittedAt truncated } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let output = &json["runs"]["results"][0]["finalOutput"];
        assert_eq!(output["content"], "the answer");
        assert_eq!(output["format"], "markdown");
        assert_eq!(output["stage"], "output");
        assert_eq!(output["submittedAt"], 1_788_924_600i64);
        assert_eq!(output["truncated"], false);
    })
    .await;
}

/// A run that has submitted nothing says so with nulls, and its detail fields
/// are empty rather than absent.
#[tokio::test]
async fn a_run_with_nothing_recorded_reads_as_empty() {
    crate::runstate::with_isolated_runs_dir_async("graphql-empty-detail", |_d| async move {
        create_run(&meta_at("coder-1788924523-bare00", 100)).expect("run written");

        let answer = run_query(
            "{ runs { results { finalOutput { content } context { totalTokens }
                                stages { results { name } total cursor }
                                waitReason { reason }
                                flags { emptyOutput modifiedFileCount } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["results"][0];
        assert!(node["finalOutput"].is_null(), "nothing submitted");
        assert!(node["context"].is_null(), "no window written yet");
        assert!(node["waitReason"].is_null(), "not parked");
        assert_eq!(node["stages"]["results"].as_array().map(Vec::len), Some(0));
        assert_eq!(node["stages"]["total"], 0, "an empty page counts as none");
        assert!(node["stages"]["cursor"].is_null(), "there is no page two");
        assert_eq!(node["flags"]["modifiedFileCount"], 0);

        // A cursor nothing minted for this listing is refused rather than
        // resumed from whatever it decodes to.
        let refused =
            run_query(r#"{ runs { results { stages(after: "not-a-cursor") { total } } } }"#).await;
        assert!(!refused.errors.is_empty(), "a cursor is checked");
    })
    .await;
}

/// A run's live window, read from the run's own directory.
#[tokio::test]
async fn a_run_carries_its_context_window() {
    crate::runstate::with_isolated_runs_dir_async("graphql-run-window", |_d| async move {
        let meta = meta_at("coder-1788924523-win000", 100);
        create_run(&meta).expect("run written");
        crate::runstate::write_context_snapshot(
            &meta.run_id,
            &leviath_core::run_meta::ContextSnapshot {
                stage_name: "build".to_string(),
                total_tokens: 42,
                max_tokens: 8_000,
                regions: vec![leviath_core::run_meta::RegionSnapshot {
                    name: "plan".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 42,
                    max_tokens: 2_000,
                    description: None,
                    entries: Vec::new(),
                }],
            },
        )
        .expect("window written");

        let answer = run_query(
            "{ runs { results { context { totalTokens maxTokens stageName
                                         regions { name tokens } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let window = &json["runs"]["results"][0]["context"];
        assert_eq!(window["totalTokens"], 42);
        assert_eq!(window["stageName"], "build");
        assert_eq!(window["regions"][0]["name"], "plan");
    })
    .await;
}

/// A run whose blueprint snapshot will not parse reports that, rather than
/// answering with a blueprint it had to invent.
#[tokio::test]
async fn a_snapshot_that_will_not_parse_is_reported() {
    crate::runstate::with_isolated_runs_dir_async("graphql-bad-snapshot", |_d| async move {
        let meta = meta_at("coder-1788924523-bad000", 100);
        create_run(&meta).expect("run written");
        std::fs::write(
            crate::commands::serve::core::blueprints::run_dir(&meta.run_id)
                .join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
            "this is not a manifest",
        )
        .expect("snapshot written");

        let answer = run_query("{ runs { results { blueprint { name } } } }").await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("will not parse"),
            "{}",
            error.message
        );
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"INTERNAL\"".to_string())
        );
    })
    .await;
}

/// A run's children are the run listing with the parent preset: same filter,
/// same order, same keyset cursor.
///
/// A fan-out of two hundred workers is the case this exists for: the whole
/// level in one response is what a connection avoids.
#[tokio::test]
async fn a_runs_children_are_paged() {
    crate::runstate::with_isolated_runs_dir_async("graphql-children", |_d| async move {
        create_run(&meta_at("root", 100)).expect("run written");
        for i in 0..3 {
            let mut child = meta_at(&format!("worker-{i}"), 200 + i);
            child.parent_run_id = Some("root".to_string());
            create_run(&child).expect("run written");
        }

        let answer = run_query(
            r#"{ runs(filter: { id: { eq: "root" } }) { results {
                   children(first: 2) { total cursor results { id parentId } }
                 } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let children = &json["runs"]["results"][0]["children"];
        assert_eq!(children["total"], 3);
        assert_eq!(children["results"].as_array().map(Vec::len), Some(2));
        assert_eq!(children["results"][0]["parentId"], "root");
        assert_eq!(children["results"][0]["id"], "worker-2", "newest first");
        let cursor = children["cursor"].as_str().expect("a level to resume");

        let rest = run_query(&format!(
            r#"{{ runs(filter: {{ id: {{ eq: "root" }} }}) {{ results {{
                   children(first: 2, after: "{cursor}") {{ cursor results {{ id }} }}
                 }} }} }}"#
        ))
        .await;
        assert!(rest.errors.is_empty(), "{:?}", rest.errors);
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        let children = &json["runs"]["results"][0]["children"];
        assert_eq!(children["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(children["results"][0]["id"], "worker-0");
        assert!(children["cursor"].is_null(), "that was the last of them");

        // The child listing takes the run filter too.
        let narrowed = run_query(
            r#"{ runs(filter: { id: { eq: "root" } }) { results {
                   children(filter: { id: { eq: "worker-1" } }) { results { id } }
                 } } }"#,
        )
        .await;
        assert!(narrowed.errors.is_empty(), "{:?}", narrowed.errors);
        let json = serde_json::to_value(&narrowed.data).expect("data serializes");
        let children = &json["runs"]["results"][0]["children"];
        assert_eq!(children["results"][0]["id"], "worker-1");
    })
    .await;
}

/// The subtree roll-up covers every run below, at any depth.
///
/// A parent that spent little and whose workers spent a great deal is not a
/// cheap run, and this is the figure that says so.
#[tokio::test]
async fn the_tree_status_rolls_up_the_whole_subtree() {
    crate::runstate::with_isolated_runs_dir_async("graphql-tree-status", |_d| async move {
        let mut root = meta_at("root", 100);
        root.prompt_tokens = 10;
        create_run(&root).expect("run written");
        let mut child = meta_at("worker", 200);
        child.parent_run_id = Some("root".to_string());
        child.prompt_tokens = 100;
        create_run(&child).expect("run written");
        let mut grandchild = meta_at("helper", 300);
        grandchild.parent_run_id = Some("worker".to_string());
        grandchild.prompt_tokens = 1_000;
        create_run(&grandchild).expect("run written");

        let answer = run_query(
            r#"{ runs(filter: { id: { eq: "root" } }) { results {
                   treeStatus { depth descendantCount rollup { promptTokens } }
                 } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let tree = &json["runs"]["results"][0]["treeStatus"];
        assert_eq!(tree["depth"], 2, "two levels below the root");
        assert_eq!(tree["descendantCount"], 2);
        assert_eq!(tree["rollup"]["promptTokens"], 1_110);
    })
    .await;
}

/// A run with no children reports a bare tree rather than nothing.
#[tokio::test]
async fn a_leaf_run_has_a_tree_of_its_own() {
    crate::runstate::with_isolated_runs_dir_async("graphql-tree-leaf", |_d| async move {
        let mut leaf = meta_at("leaf", 100);
        leaf.prompt_tokens = 7;
        create_run(&leaf).expect("run written");

        let answer = run_query(
            r#"{ runs { results { treeStatus { depth descendantCount
                                          rollup { promptTokens } } } } }"#,
        )
        .await;
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let tree = &json["runs"]["results"][0]["treeStatus"];
        assert_eq!(tree["depth"], 0);
        assert_eq!(tree["descendantCount"], 0);
        assert_eq!(tree["rollup"]["promptTokens"], 7);
    })
    .await;
}

/// The log selectors: one stage, every stage, and the two streams.
#[tokio::test]
async fn logs_read_one_stage_or_every_stage() {
    crate::runstate::with_isolated_runs_dir_async("graphql-logs", |_d| async move {
        let meta = meta_at("coder-1788924523-log000", 100);
        create_run(&meta).expect("run written");
        crate::runstate::append_stage_output(&meta.run_id, 0, "first stage output\n");
        crate::runstate::append_stage_log(&meta.run_id, 0, "[tool] read_file\n");

        let output = run_query("{ runs { results { logs(stage: { index: 0 }) } } }").await;
        assert!(output.errors.is_empty(), "{:?}", output.errors);
        let json = serde_json::to_value(&output.data).expect("data serializes");
        assert!(
            json["runs"]["results"][0]["logs"]
                .as_str()
                .unwrap_or_default()
                .contains("first stage output"),
            "{json}"
        );

        let operational =
            run_query("{ runs { results { logs(stage: { index: 0 }, stream: OPERATIONAL) } } }")
                .await;
        assert!(operational.errors.is_empty(), "{:?}", operational.errors);
        let json = serde_json::to_value(&operational.data).expect("data serializes");
        assert!(
            json["runs"]["results"][0]["logs"]
                .as_str()
                .unwrap_or_default()
                .contains("[tool] read_file"),
            "{json}"
        );

        let every =
            run_query("{ runs { results { logs(stage: { all: true }, tailBytes: 100) } } }").await;
        assert!(every.errors.is_empty(), "{:?}", every.errors);

        // Omitted, the selector means the stage the run is on now.
        let current = run_query("{ runs { results { logs } } }").await;
        assert!(current.errors.is_empty(), "{:?}", current.errors);
    })
    .await;
}

/// Asking for one stage and every stage at once is a contradiction the schema
/// itself refuses, and a negative index or window is one the resolver does.
#[tokio::test]
async fn the_log_selectors_refuse_a_contradiction() {
    crate::runstate::with_isolated_runs_dir_async("graphql-logs-refused", |_d| async move {
        create_run(&meta_at("coder-1788924523-bad999", 100)).expect("run written");

        // `stage` takes exactly one of its fields, so naming both is refused
        // before anything runs rather than by the resolver.
        let both = run_query("{ runs { results { logs(stage: { index: 0, all: true }) } } }").await;
        assert!(
            both.errors
                .first()
                .expect("a refusal")
                .message
                .contains("exactly one field"),
            "{:?}",
            both.errors
        );

        let negative = run_query("{ runs { results { logs(stage: { index: -1 }) } } }").await;
        assert!(
            negative
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("negative"),
            "{:?}",
            negative.errors
        );

        let window = run_query("{ runs { results { logs(tailBytes: -1) } } }").await;
        assert!(
            window
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("negative"),
            "{:?}",
            window.errors
        );
    })
    .await;
}

/// A run's parts come back as metadata plus a signed link, never as bytes.
///
/// Bytes in a query answer would be base64 in a JSON string, which is both
/// larger and unusable by an `<img>`. The link is what a page actually needs.
#[tokio::test]
async fn a_runs_parts_carry_signed_links_rather_than_bytes() {
    crate::runstate::with_isolated_runs_dir_async("graphql-blobs", |_d| async move {
        let meta = meta_at("coder-1788924523-blob00", 100);
        create_run(&meta).expect("run written");
        // A stored part, named by the run's context the way a real one is.
        let registry = leviath_core::mime::MimeRegistry::builtin();
        use leviath_core::mime::BlobStore as _;
        let store = leviath_runtime::blob_store::FsBlobStore::new(crate::runstate::runs_dir());
        let picture = leviath_core::mime::Blob::new(
            leviath_core::mime::MimeType::parse("image/png").expect("a mime type"),
            b"\x89PNG\r\n\x1a\n".to_vec(),
        );
        let stored = store
            .put(&meta.run_id, &picture, &registry)
            .expect("stored");
        let mut entry = leviath_core::run_meta::RegionEntrySnapshot {
            content: leviath_core::region::EntryContent::from_parts(vec![
                leviath_core::mime::Part::stored(stored.clone()).named("shot.png"),
            ]),
            tokens: 1,
            kind: Default::default(),
            metadata: None,
            key: None,
            reasoning: None,
            taint: Default::default(),
        };
        entry.tokens = 10;
        crate::runstate::write_context_snapshot(
            &meta.run_id,
            &leviath_core::run_meta::ContextSnapshot {
                stage_name: "build".to_string(),
                total_tokens: 10,
                max_tokens: 100,
                regions: vec![leviath_core::run_meta::RegionSnapshot {
                    name: "files".to_string(),
                    kind: "temporary".to_string(),
                    current_tokens: 10,
                    max_tokens: 100,
                    description: None,
                    entries: vec![entry],
                }],
            },
        )
        .expect("window written");

        let answer = run_query(
            "{ runs { results { blobs { results
                 { sha256 mimeType name size stored regions url } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let blob = &json["runs"]["results"][0]["blobs"]["results"][0];
        assert_eq!(blob["mimeType"], "image/png");
        assert_eq!(blob["name"], "shot.png");
        assert_eq!(blob["stored"], true);
        assert_eq!(blob["regions"][0], "files");
        let url = blob["url"].as_str().expect("a link");
        assert!(url.contains("/blobs/"), "{url}");
        assert!(url.contains("sig="), "it carries its own grant: {url}");

        // The parts are what the listing filters on, not the page of them and
        // not the link: "every run holding a picture" is one request.
        let holding = run_query(
            r#"{ runs(filter: { blobs: { some: { mimeType: { eq: "image/png" } } } })
                 { results { id } } }"#,
        )
        .await;
        assert!(holding.errors.is_empty(), "{:?}", holding.errors);
        assert_eq!(
            ids_of(&holding.data, "runs"),
            vec!["coder-1788924523-blob00".to_string()]
        );

        let audio = run_query(
            r#"{ runs(filter: { blobs: { some: { mimeType: { eq: "audio/wav" } } } })
                 { results { id } } }"#,
        )
        .await;
        assert!(audio.errors.is_empty(), "{:?}", audio.errors);
        assert!(ids_of(&audio.data, "runs").is_empty(), "nothing holds one");
    })
    .await;
}

/// The files a run handed back are read as a page and filtered as a list, and
/// the filter settles from the run's own record.
#[tokio::test]
async fn the_files_a_run_handed_back_page_and_filter() {
    crate::runstate::with_isolated_runs_dir_async("graphql-artifacts", |_d| async move {
        let mut meta = meta_at("shipped", 100);
        meta.final_output = Some(leviath_core::FinalOutputDescriptor {
            format: None,
            stage: "output".to_string(),
            submitted_at: 1_788_924_600,
            bytes: 4,
            truncated: false,
            artifacts: vec![leviath_core::output::Artifact {
                name: "scene".to_string(),
                path: "out/scene.glb".to_string(),
                mime_type: leviath_core::mime::MimeType::parse("model/gltf-binary")
                    .expect("a mime type"),
                size: 2_048,
                sha256: "a".repeat(64),
            }],
        });
        create_run(&meta).expect("run written");
        create_run(&meta_at("nothing", 200)).expect("run written");

        let page = run_query(
            r#"{ runs(filter: { id: { eq: "shipped" } }) { results
                 { artifacts { results { name path mimeType size sha256 url } total } } } }"#,
        )
        .await;
        assert!(page.errors.is_empty(), "{:?}", page.errors);
        let json = serde_json::to_value(&page.data).expect("data serializes");
        let artifacts = &json["runs"]["results"][0]["artifacts"];
        assert_eq!(artifacts["total"], 1);
        let file = &artifacts["results"][0];
        assert_eq!(file["name"], "scene");
        assert_eq!(file["path"], "out/scene.glb");
        assert_eq!(file["mimeType"], "model/gltf-binary");
        assert!(
            file["url"].as_str().unwrap_or_default().contains("sig="),
            "the resolver still mints a link: {file}"
        );

        let mark = read_mark();
        let matched = run_query(
            r#"{ runs(filter: { artifacts: { some: { name: { eq: "scene" } } } })
                 { results { id } } }"#,
        )
        .await;
        assert!(matched.errors.is_empty(), "{:?}", matched.errors);
        assert_eq!(ids_of(&matched.data, "runs"), vec!["shipped".to_string()]);
        assert!(
            reads_since(mark).is_empty(),
            "the submission record is already in memory: {:?}",
            reads_since(mark)
        );

        let none = run_query(
            r#"{ runs(filter: { artifacts: { none: { name: { eq: "scene" } } } })
                 { results { id } } }"#,
        )
        .await;
        assert_eq!(ids_of(&none.data, "runs"), vec!["nothing".to_string()]);
    })
    .await;
}

/// A file link names the file and the grant, and says when it is a download.
#[tokio::test]
async fn a_file_link_carries_its_path_and_its_grant() {
    crate::runstate::with_isolated_runs_dir_async("graphql-file-url", |_d| async move {
        create_run(&meta_at("coder-1788924523-file00", 100)).expect("run written");

        let answer = run_query(
            r#"{ runs { results {
                   inline: fileUrl(path: "out.png")
                   saved: fileUrl(path: "out.png", download: true)
                 } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["results"][0];
        let inline = node["inline"].as_str().expect("a link");
        assert!(inline.contains("/files/raw?"), "{inline}");
        assert!(inline.contains("path=out.png"), "{inline}");
        assert!(inline.contains("sig="), "{inline}");
        assert!(
            !inline.contains("download=1"),
            "inline by default: {inline}"
        );
        let saved = node["saved"].as_str().expect("a link");
        assert!(saved.contains("download=1"), "{saved}");
    })
    .await;
}

/// The machine's own state: how it is configured, and what it can do.
#[tokio::test]
async fn the_config_field_answers_without_secrets() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            "{ config { routing { defaultProvider providerOrder overrideModel fallbackModel }
                        providers { id name auth isEnabled hasKey baseUrl region
                          options { __typename } }
                        allowsFileUploads blueprintPaths mcpServerCount
                        server { apiVersion capabilities isAdminEnabled
                          limits { maxPageSize maxIds maxUploadBytes requestTimeoutSecs } }
                        health { error { message } savedAt }
                        yoloFile { path exists error } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let config = &json["config"];
        assert!(
            config["server"]["capabilities"]
                .as_array()
                .expect("capabilities")
                .iter()
                .any(|c| c == "graphql"),
            "this server announces the surface a client is reading it over"
        );
        assert_eq!(config["server"]["limits"]["maxPageSize"], 200);
        assert!(
            config["server"]["limits"]["maxIds"]
                .as_i64()
                .unwrap_or_default()
                > 0
        );
        assert!(
            config["server"]["apiVersion"]
                .as_str()
                .unwrap_or_default()
                .len()
                > 2,
            "{config}"
        );
        // Every provider this build knows has a row whether it is set up or
        // not, so a settings screen draws a stable list rather than one that
        // grows and shrinks under it.
        let providers = config["providers"].as_array().expect("the providers");
        assert_eq!(providers.len(), 10);
        assert!(
            providers.iter().all(|provider| provider["hasKey"] == false),
            "nothing is configured in an isolated home, which is a state rather \
             than a failure"
        );
        assert!(
            providers
                .iter()
                .any(|provider| provider["options"]["__typename"] == "CodexOptionsOutput"),
            "the one provider with settings of its own carries them: {providers:?}"
        );
        assert!(
            !serde_json::to_string(config)
                .expect("config serializes")
                .contains("key\":\""),
            "no key values cross the wire"
        );
    })
    .await;
}

/// The diagnostics report. A failing check is a finding, not a request error.
#[tokio::test]
async fn the_doctor_reports_its_checks() {
    // The checks read the real config path unless one is staked out for them,
    // which the repo's own guard insists on: an unisolated read races every
    // other environment-touching test.
    crate::config::with_isolated_config_path_async("graphql-doctor", |_path| async move {
        let answer = run_query("{ doctor { ok isLive checks { name ok detail } } }").await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        // A query dials nothing, so this report never can have: `checkMachine`
        // is the mutation that does, and it says so with the same field.
        assert_eq!(json["doctor"]["isLive"], false);
        let checks = json["doctor"]["checks"].as_array().expect("checks");
        assert!(!checks.is_empty(), "something was checked");
        assert!(
            checks
                .iter()
                .all(|c| !c["name"].as_str().unwrap_or_default().is_empty()),
            "each one says what it checked"
        );
    })
    .await;
}

/// `checkMachine`'s own report differs from the offline one only in
/// `isLive`: same shape, so a client renders one view of either.
#[test]
fn the_live_report_says_it_dialled_out() {
    fn checks() -> Vec<super::super::super::types::DoctorCheck> {
        vec![super::super::super::types::DoctorCheck {
            name: "provider".to_string(),
            ok: true,
            detail: "reachable".to_string(),
            elapsed_ms: Some(12),
        }]
    }
    let report = super::live_doctor_report(checks());
    assert!(report.ok);
    assert!(report.is_live, "the live report says it dialled out");
    assert_eq!(report.checks[0].name, "provider");

    let offline = super::machine::doctor_report(checks());
    assert!(!offline.is_live, "the offline report never dialled out");
}

/// The MCP servers, the yolo profiles, the mime rows and the scripts, from a
/// machine with none of them configured.
///
/// Empty is the honest answer here, and each field says where it read from
/// rather than implying the file is broken.
#[tokio::test]
async fn the_machine_listings_answer_for_a_bare_install() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            "{ mcpServers { results { name transport endpoint auth } }
               yoloProfiles { total results { name default } }
               config { yoloFile { path exists error } }
               mimeRows(first: 200) { results { mimeType origin blueprintName family isText
                   extensions } }
               scripts { results { kind name scope blueprintName } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(
            json["mcpServers"]["results"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(json["yoloProfiles"]["total"], 0, "no profiles file yet");
        let yolo_file = &json["config"]["yoloFile"];
        assert_eq!(yolo_file["exists"], false, "and no file to read them from");
        assert!(yolo_file["error"].is_null(), "and no failure");
        assert!(
            yolo_file["path"]
                .as_str()
                .unwrap_or_default()
                .ends_with("yolo.toml"),
            "it says where it looked"
        );
        // The built-in mime rows are always there: they are compiled in.
        let mime = json["mimeRows"]["results"].as_array().expect("mime rows");
        assert!(
            mime.iter().any(|row| row["mimeType"] == "image/png"),
            "the built-in rows are listed"
        );
        assert!(
            mime.iter().all(|row| row["origin"] == "BUILTIN"),
            "a bare install has only the compiled-in layer"
        );
        assert!(
            mime.iter().all(|row| row["blueprintName"].is_null()),
            "and none of them belongs to a blueprint"
        );
        assert!(json["scripts"]["results"].is_array());
    })
    .await;
}

/// The directory picker lists directories, and says where "up" and "home" are.
#[tokio::test]
async fn the_directory_picker_lists_directories() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("visible")).expect("a child");
        std::fs::create_dir_all(dir.path().join(".hidden")).expect("a hidden child");
        std::fs::write(dir.path().join("a-file.txt"), "x").expect("a file");
        let path = dir.path().to_string_lossy().into_owned();

        let answer = run_query_for_path(
            "query Dirs($path: String!) { directory(path: $path)
               { path parent home cwd entries } }",
            &path,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let listing = &json["directory"];
        let entries = listing["entries"].as_array().expect("entries");
        assert!(entries.iter().any(|e| e == "visible"));
        assert!(
            !entries.iter().any(|e| e == ".hidden"),
            "hidden ones are left out unless asked for"
        );
        assert!(
            !entries.iter().any(|e| e == "a-file.txt"),
            "a file is not a directory"
        );
        assert!(listing["parent"].is_string(), "up one level");
        assert!(!listing["home"].as_str().unwrap_or_default().is_empty());

        let with_hidden = run_query_for_path(
            "query Dirs($path: String!) { directory(path: $path, includeHidden: true)
               { entries } }",
            &path,
        )
        .await;
        let json = serde_json::to_value(&with_hidden.data).expect("data serializes");
        assert!(
            json["directory"]["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .any(|e| e == ".hidden"),
            "asked for, they are there"
        );
    })
    .await;
}

/// A path that is not a directory, or is not there, says which.
#[tokio::test]
async fn the_directory_picker_refuses_what_it_cannot_list() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let relative = run_query(r#"{ directory(path: "relative/path") { path } }"#).await;
        assert!(
            relative
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("must be absolute"),
            "{:?}",
            relative.errors
        );

        // An absolute path nothing is at, written the way this platform writes
        // one: a leading slash is not absolute on Windows.
        let nowhere = std::path::Path::new(&home)
            .join("nowhere")
            .join("at")
            .join("all")
            .to_string_lossy()
            .into_owned();
        let missing = run_query_for_path(
            "query Dirs($path: String!) { directory(path: $path) { path } }",
            &nowhere,
        )
        .await;
        assert_eq!(
            missing
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string())
        );
    })
    .await;
}

/// The tree filters each answer a different question, and two at once is a
/// client that built its query wrong rather than a set to guess at.
#[tokio::test]
async fn the_tree_filters_answer_different_questions() {
    crate::runstate::with_isolated_runs_dir_async("graphql-tree-filters", |_dir| async move {
        // root -> worker -> grandchild, plus an unrelated run of another
        // blueprint.
        let mut root = named("root", "coder");
        root.started_at = 100;
        crate::runstate::create_run(&root).expect("run written");
        let mut worker = named("worker", "coder");
        worker.parent_run_id = Some("root".to_string());
        worker.started_at = 200;
        crate::runstate::create_run(&worker).expect("run written");
        let mut grandchild = named("grandchild", "coder");
        grandchild.parent_run_id = Some("worker".to_string());
        grandchild.started_at = 300;
        crate::runstate::create_run(&grandchild).expect("run written");
        let mut other = named("other", "researcher");
        other.started_at = 400;
        crate::runstate::create_run(&other).expect("run written");

        // Direct children: one level.
        let answer =
            run_query(r#"{ runs(filter: { parentId: { eq: "root" } }) { results { id } } }"#).await;
        assert_eq!(ids_of(&answer.data, "runs"), vec!["worker".to_string()]);

        // The whole subtree: every level, and not the root itself.
        let answer =
            run_query(r#"{ runs(filter: { ancestorIds: { has: "root" } }) { results { id } } }"#)
                .await;
        let mut under = ids_of(&answer.data, "runs");
        under.sort();
        assert_eq!(under, vec!["grandchild".to_string(), "worker".to_string()]);

        // The run above one of them, reached as a relation rather than an id.
        let answer = run_query(
            r#"{ runs(filter: { parent: { blueprintName: { eq: "coder" } } }) { results { id } } }"#,
        )
        .await;
        let mut of_coder = ids_of(&answer.data, "runs");
        of_coder.sort();
        assert_eq!(
            of_coder,
            vec!["grandchild".to_string(), "worker".to_string()]
        );

        // Roots, and its mirror.
        let answer =
            run_query("{ runs(filter: { parentId: { isNull: true } }) { results { id } } }").await;
        let mut roots = ids_of(&answer.data, "runs");
        roots.sort();
        assert_eq!(roots, vec!["other".to_string(), "root".to_string()]);
        let answer =
            run_query("{ runs(filter: { parentId: { isNull: false } }) { results { id } } }").await;
        let mut subs = ids_of(&answer.data, "runs");
        subs.sort();
        assert_eq!(subs, vec!["grandchild".to_string(), "worker".to_string()]);

        // By blueprint name, which composes with the rest.
        let answer = run_query(
            r#"{ runs(filter: { blueprintName: { eq: "researcher" } }) { results { id } total } }"#,
        )
        .await;
        assert_eq!(ids_of(&answer.data, "runs"), vec!["other".to_string()]);
        let answer = run_query(
            r#"{ runs(filter: { blueprintName: { eq: "coder" }, parentId: { isNull: false } })
                 { results { id } } }"#,
        )
        .await;
        assert_eq!(ids_of(&answer.data, "runs").len(), 2);
        // A blueprint nothing matches is an empty page, not a refusal: a
        // blueprint with no runs yet is an ordinary answer.
        let answer =
            run_query(r#"{ runs(filter: { blueprintName: { eq: "nope" } }) { total } }"#).await;
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 0);
    })
    .await;
}

/// Two parentage fields on one filter object intersect, because every field
/// set on one object has to hold.
///
/// One run's children that are also somewhere under it is that run's children,
/// and a scope that contradicts a parent is an empty page rather than a
/// refusal: a predicate nothing satisfies is an answer.
#[tokio::test]
async fn two_parentage_fields_on_one_object_intersect() {
    crate::runstate::with_isolated_runs_dir_async("graphql-parentage-and", |_d| async move {
        create_run(&meta_at("root", 100)).expect("run written");
        let mut worker = meta_at("worker", 200);
        worker.parent_run_id = Some("root".to_string());
        create_run(&worker).expect("run written");

        let both = run_query(
            r#"{ runs(filter: { parentId: { eq: "root" }, ancestorIds: { has: "root" } })
                 { results { id } } }"#,
        )
        .await;
        assert!(both.errors.is_empty(), "{:?}", both.errors);
        assert_eq!(ids_of(&both.data, "runs"), vec!["worker".to_string()]);

        let contradiction = run_query(
            r#"{ runs(filter: { and: [{ parentId: { eq: "root" } },
                                      { parentId: { isNull: true } }] }) { total } }"#,
        )
        .await;
        assert!(
            contradiction.errors.is_empty(),
            "{:?}",
            contradiction.errors
        );
        let json = serde_json::to_value(&contradiction.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 0);
    })
    .await;
}

/// The config says whether the admin mutations will run, so a settings screen
/// does not have to offer a save that answers `FORBIDDEN`.
#[tokio::test]
async fn the_config_says_whether_admin_is_open() {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    for allow_admin in [false, true] {
        let schema = async_graphql::Schema::build(
            Query,
            async_graphql::EmptyMutation,
            async_graphql::EmptySubscription,
        )
        .data(state.clone())
        .data(crate::commands::serve::graphql::admin::AdminAccess(
            allow_admin,
        ))
        .finish();
        let answer = schema
            .execute(async_graphql::Request::new(
                "{ config { server { isAdminEnabled } } }",
            ))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["config"]["server"]["isAdminEnabled"], allow_admin);
    }
}

/// The listings that read the machine rather than the run store.
///
/// Each is a walk of a directory or a table on disk, so these arrange one and
/// then ask: what is asserted is that the walk reaches the schema, with the
/// fields a console renders.
mod machine_listings {
    use super::*;

    /// Scripts come back with where each one came from, and an agent narrows the
    /// answer to that agent's own plus the global ones.
    #[tokio::test]
    async fn the_scripts_listing_reports_where_each_came_from() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let global = home.join(".leviath").join("tools");
            std::fs::create_dir_all(&global).expect("the tools directory");
            std::fs::write(
                global.join("summarize.rhai"),
                "// @tool summarize\n// @description sums up\n\"ok\"",
            )
            .expect("a tool");

            let answer =
                run_query("{ scripts { results { id kind name scope blueprintName } } }").await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let scripts = json["scripts"]["results"].as_array().expect("the scripts");
            let tool = scripts
                .iter()
                .find(|script| script["name"] == "summarize")
                .expect("the tool that was just written");
            assert_eq!(tool["kind"], "TOOL");
            assert_eq!(tool["scope"], "GLOBAL");
            assert!(
                tool["blueprintName"].is_null(),
                "a global tool belongs to nobody"
            );

            // One script, by the three things that name it.
            let one = run_query(
                r#"{ script(ref: { kind: TOOL, name: "summarize" }) { id kind name scope } }"#,
            )
            .await;
            assert!(one.errors.is_empty(), "{:?}", one.errors);
            let json = serde_json::to_value(&one.data).expect("data serializes");
            assert_eq!(json["script"]["name"], "summarize");
            assert_eq!(json["script"]["scope"], "GLOBAL");

            // A reference nothing is filed under answers null rather than
            // failing: a client that guessed a kind still gets an answer.
            let missing =
                run_query(r#"{ script(ref: { kind: STAGE_HOOK, name: "summarize" }) { id } }"#)
                    .await;
            assert!(missing.errors.is_empty(), "{:?}", missing.errors);
            let json = serde_json::to_value(&missing.data).expect("data serializes");
            assert!(json["script"].is_null());
        })
        .await;
    }

    /// Two scripts: the root listing orders both ways, resumes a cursor and
    /// refuses one minted for a different order.
    #[tokio::test]
    async fn the_scripts_listing_orders_pages_and_refuses_a_foreign_cursor() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let global = home.join(".leviath").join("tools");
            std::fs::create_dir_all(&global).expect("the tools directory");
            std::fs::write(
                global.join("alpha.rhai"),
                "// @tool alpha\n// @description first\n\"ok\"",
            )
            .expect("a tool");
            std::fs::write(
                global.join("beta.rhai"),
                "// @tool beta\n// @description second\n\"ok\"",
            )
            .expect("a tool");

            let ascending = run_query(
                "{ scripts(orderBy: [{ field: ID, direction: ASC }]) { results { name } total } }",
            )
            .await;
            assert!(ascending.errors.is_empty(), "{:?}", ascending.errors);
            let json = serde_json::to_value(&ascending.data).expect("data serializes");
            let names: Vec<String> = json["scripts"]["results"]
                .as_array()
                .expect("both scripts")
                .iter()
                .map(|script| script["name"].as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
            assert_eq!(json["scripts"]["total"], 2);

            let descending = run_query(
                "{ scripts(orderBy: [{ field: ID, direction: DESC }]) { results { name } } }",
            )
            .await;
            let json = serde_json::to_value(&descending.data).expect("data serializes");
            let mut desc_names: Vec<String> = json["scripts"]["results"]
                .as_array()
                .expect("descending")
                .iter()
                .map(|script| script["name"].as_str().unwrap_or_default().to_string())
                .collect();
            desc_names.reverse();
            assert_eq!(names, desc_names, "the same order, read the other way");

            let page = run_query(
                "{ scripts(first: 1, orderBy: [{ field: ID, direction: ASC }]) \
                   { results { name } cursor } }",
            )
            .await;
            let json = serde_json::to_value(&page.data).expect("data serializes");
            let cursor = json["scripts"]["cursor"]
                .as_str()
                .expect("a second page follows")
                .to_string();
            let rest = run_query(&format!(
                r#"{{ scripts(first: 1, after: "{cursor}",
                     orderBy: [{{ field: ID, direction: ASC }}]) {{ results {{ name }} }} }}"#
            ))
            .await;
            assert!(rest.errors.is_empty(), "{:?}", rest.errors);
            let json = serde_json::to_value(&rest.data).expect("data serializes");
            assert_eq!(json["scripts"]["results"][0]["name"], "beta");

            let crossed = run_query(&format!(
                r#"{{ scripts(after: "{cursor}",
                     orderBy: [{{ field: ID, direction: DESC }}]) {{ total }} }}"#
            ))
            .await;
            assert!(!crossed.errors.is_empty(), "a cursor is bound to its order");
        })
        .await;
    }

    /// The tools listing carries the group tokens a stage can name, beside the
    /// tools themselves.
    #[tokio::test]
    async fn the_tools_listing_carries_the_group_tokens() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let answer = run_query(
                "{ tools(first: 200, filter: { origin: { eq: BUILTIN } }) {
                     results { name origin description
                       ... on ScriptToolOutput { path blueprint requires } }
                     skipped { path reason } total }
                   toolGroups { name description } }",
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let tools = json["tools"]["results"].as_array().expect("the tools");
            assert!(!tools.is_empty(), "a build ships built-in tools");
            assert!(
                tools
                    .iter()
                    .all(|tool| tool["name"].as_str().is_some_and(|n| !n.is_empty()))
            );
            assert!(
                tools.iter().all(|tool| tool["origin"] == "BUILTIN"),
                "the filter reaches the listing: {tools:?}"
            );
            let groups = json["toolGroups"].as_array().expect("the groups");
            assert!(
                groups.iter().any(|group| group["name"] == "@builtin"),
                "the tokens a stage can name: {groups:?}"
            );
            assert_eq!(
                json["tools"]["total"].as_i64(),
                Some(i64::try_from(tools.len()).unwrap_or(0)),
                "total counts the whole listing, not the page"
            );
        })
        .await;
    }

    /// The tools listing orders both ways, resumes a cursor and refuses one
    /// minted for a different order: a build ships more than one built-in
    /// tool, so the walk actually has something to reorder.
    #[tokio::test]
    async fn the_tools_listing_orders_pages_and_refuses_a_foreign_cursor() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let ascending = run_query(
                "{ tools(first: 200, orderBy: [{ field: NAME, direction: ASC }]) \
                   { results { name } } }",
            )
            .await;
            assert!(ascending.errors.is_empty(), "{:?}", ascending.errors);
            let json = serde_json::to_value(&ascending.data).expect("data serializes");
            let names: Vec<String> = json["tools"]["results"]
                .as_array()
                .expect("built-in tools")
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
                .collect();
            assert!(names.len() >= 2, "more than one built-in tool: {names:?}");

            let descending = run_query(
                "{ tools(first: 200, orderBy: [{ field: NAME, direction: DESC }]) \
                   { results { name } } }",
            )
            .await;
            let json = serde_json::to_value(&descending.data).expect("data serializes");
            let mut desc_names: Vec<String> = json["tools"]["results"]
                .as_array()
                .expect("descending")
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
                .collect();
            desc_names.reverse();
            assert_eq!(names, desc_names, "the same order, read the other way");

            let page = run_query(
                "{ tools(first: 1, orderBy: [{ field: NAME, direction: ASC }]) \
                   { results { name } cursor } }",
            )
            .await;
            let json = serde_json::to_value(&page.data).expect("data serializes");
            let cursor = json["tools"]["cursor"]
                .as_str()
                .expect("more pages follow")
                .to_string();
            let rest = run_query(&format!(
                r#"{{ tools(first: 200, after: "{cursor}",
                     orderBy: [{{ field: NAME, direction: ASC }}]) {{ results {{ name }} }} }}"#
            ))
            .await;
            assert!(rest.errors.is_empty(), "{:?}", rest.errors);
            let json = serde_json::to_value(&rest.data).expect("data serializes");
            let resumed: Vec<String> = json["tools"]["results"]
                .as_array()
                .expect("the rest")
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(resumed, names[1..], "no repeat and nothing skipped");

            let crossed = run_query(&format!(
                r#"{{ tools(after: "{cursor}",
                     orderBy: [{{ field: NAME, direction: DESC }}]) {{ total }} }}"#
            ))
            .await;
            assert!(!crossed.errors.is_empty(), "a cursor is bound to its order");
        })
        .await;
    }

    /// The MCP servers come back from the config, with their transport and auth
    /// state.
    #[tokio::test]
    async fn the_mcp_servers_come_from_the_config() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let paths = crate::commands::serve::mcp::AdminPaths {
                config: home.join("config.toml"),
                store: home.join("mcp-auth.json"),
                grants: home.join("grants.json"),
            };
            std::fs::write(
                &paths.config,
                "[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n",
            )
            .expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    let answer = run_query(
                        r#"{ mcpServers { results { id name transport endpoint configError auth } }
                             mcpServer(name: "docs") { name transport }
                             missing: mcpServer(name: "nope") { name } }"#,
                    )
                    .await;
                    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
                    let json = serde_json::to_value(&answer.data).expect("data serializes");
                    let first = &json["mcpServers"]["results"][0];
                    assert_eq!(first["name"], "docs");
                    assert_eq!(first["transport"], "STDIO");
                    assert_eq!(first["auth"], "NOT_APPLICABLE");
                    assert!(first["configError"].is_null());
                    assert_eq!(first["endpoint"], "docs-mcp");
                    assert_eq!(first["id"], "mcpServer:docs");
                    assert_eq!(json["mcpServer"]["transport"], "STDIO");
                    assert!(
                        json["missing"].is_null(),
                        "a name nothing is configured under is an absence"
                    );
                })
                .await;
        })
        .await;
    }

    /// Two servers: the listing filters, orders both ways, resumes a cursor
    /// and refuses one minted for another order.
    #[tokio::test]
    async fn the_mcp_servers_listing_filters_orders_and_pages() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let paths = crate::commands::serve::mcp::AdminPaths {
                config: home.join("config.toml"),
                store: home.join("mcp-auth.json"),
                grants: home.join("grants.json"),
            };
            std::fs::write(
                &paths.config,
                "[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n\n\
                 [[mcp_servers]]\nname = \"search\"\ncommand = \"search-mcp\"\n",
            )
            .expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    let filtered = run_query(
                        r#"{ mcpServers(filter: { name: { eq: "docs" } }) { total } }"#,
                    )
                    .await;
                    assert!(filtered.errors.is_empty(), "{:?}", filtered.errors);
                    let json = serde_json::to_value(&filtered.data).expect("data serializes");
                    assert_eq!(json["mcpServers"]["total"], 1);

                    let ascending = run_query(
                        "{ mcpServers(orderBy: [{ field: NAME, direction: ASC }]) \
                           { results { name } } }",
                    )
                    .await;
                    let json = serde_json::to_value(&ascending.data).expect("data serializes");
                    let names: Vec<String> = json["mcpServers"]["results"]
                        .as_array()
                        .expect("both servers")
                        .iter()
                        .map(|server| server["name"].as_str().unwrap_or_default().to_string())
                        .collect();
                    assert_eq!(names, vec!["docs".to_string(), "search".to_string()]);

                    let descending = run_query(
                        "{ mcpServers(orderBy: [{ field: NAME, direction: DESC }]) \
                           { results { name } } }",
                    )
                    .await;
                    let json = serde_json::to_value(&descending.data).expect("data serializes");
                    assert_eq!(
                        json["mcpServers"]["results"][0]["name"],
                        "search",
                        "reversed"
                    );

                    let page = run_query(
                        "{ mcpServers(first: 1, orderBy: [{ field: NAME, direction: ASC }]) \
                           { results { name } cursor } }",
                    )
                    .await;
                    assert!(page.errors.is_empty(), "{:?}", page.errors);
                    let json = serde_json::to_value(&page.data).expect("data serializes");
                    let cursor = json["mcpServers"]["cursor"]
                        .as_str()
                        .expect("a second page follows")
                        .to_string();
                    let rest = run_query(&format!(
                        r#"{{ mcpServers(first: 1, after: "{cursor}",
                             orderBy: [{{ field: NAME, direction: ASC }}]) {{ results {{ name }} }} }}"#
                    ))
                    .await;
                    assert!(rest.errors.is_empty(), "{:?}", rest.errors);
                    let json = serde_json::to_value(&rest.data).expect("data serializes");
                    assert_eq!(json["mcpServers"]["results"][0]["name"], "search");

                    // Bound to the order it was minted under.
                    let crossed = run_query(&format!(
                        r#"{{ mcpServers(after: "{cursor}",
                             orderBy: [{{ field: NAME, direction: DESC }}]) {{ total }} }}"#
                    ))
                    .await;
                    assert!(
                        !crossed.errors.is_empty(),
                        "a cursor is bound to its order"
                    );
                })
                .await;
        })
        .await;
    }

    /// The compiled-in rows are enough on their own to filter, order both
    /// ways, resume a cursor and refuse one minted for another filter.
    #[tokio::test]
    async fn the_mime_rows_listing_filters_orders_and_pages() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let all = run_query(
                "{ mimeRows(first: 200, orderBy: [{ field: MIME_TYPE, direction: ASC }]) \
                   { results { mimeType } total } }",
            )
            .await;
            assert!(all.errors.is_empty(), "{:?}", all.errors);
            let json = serde_json::to_value(&all.data).expect("data serializes");
            let types: Vec<String> = json["mimeRows"]["results"]
                .as_array()
                .expect("the built-in rows")
                .iter()
                .map(|row| row["mimeType"].as_str().unwrap_or_default().to_string())
                .collect();
            assert!(types.len() >= 2, "more than one row is compiled in: {types:?}");
            let total = json["mimeRows"]["total"].as_i64().expect("a count");
            assert_eq!(total, i64::try_from(types.len()).unwrap_or(0));

            // The filter reaches the listing.
            let filtered = run_query(
                r#"{ mimeRows(filter: { mimeType: { eq: "image/png" } }) { total } }"#,
            )
            .await;
            assert!(filtered.errors.is_empty(), "{:?}", filtered.errors);
            let json = serde_json::to_value(&filtered.data).expect("data serializes");
            assert_eq!(json["mimeRows"]["total"], 1);
            let missed = run_query(
                r#"{ mimeRows(filter: { mimeType: { eq: "nothing/here" } }) { total } }"#,
            )
            .await;
            let json = serde_json::to_value(&missed.data).expect("data serializes");
            assert_eq!(json["mimeRows"]["total"], 0);

            // The other direction is the same rows, reversed.
            let descending = run_query(
                "{ mimeRows(first: 200, orderBy: [{ field: MIME_TYPE, direction: DESC }]) \
                   { results { mimeType } } }",
            )
            .await;
            let json = serde_json::to_value(&descending.data).expect("data serializes");
            let mut desc_types: Vec<String> = json["mimeRows"]["results"]
                .as_array()
                .expect("descending")
                .iter()
                .map(|row| row["mimeType"].as_str().unwrap_or_default().to_string())
                .collect();
            desc_types.reverse();
            assert_eq!(types, desc_types, "the same order, read the other way");

            // A page, then the rest, resumed from the cursor the first page
            // handed back.
            let page = run_query(
                "{ mimeRows(first: 1, orderBy: [{ field: MIME_TYPE, direction: ASC }]) \
                   { results { mimeType } cursor } }",
            )
            .await;
            let json = serde_json::to_value(&page.data).expect("data serializes");
            let cursor = json["mimeRows"]["cursor"]
                .as_str()
                .expect("more pages follow")
                .to_string();
            let rest = run_query(&format!(
                r#"{{ mimeRows(first: 200, after: "{cursor}",
                     orderBy: [{{ field: MIME_TYPE, direction: ASC }}]) {{ results {{ mimeType }} }} }}"#
            ))
            .await;
            assert!(rest.errors.is_empty(), "{:?}", rest.errors);
            let json = serde_json::to_value(&rest.data).expect("data serializes");
            let resumed: Vec<String> = json["mimeRows"]["results"]
                .as_array()
                .expect("the rest")
                .iter()
                .map(|row| row["mimeType"].as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(resumed, types[1..], "no repeat and nothing skipped");

            // A cursor minted for this filter is refused under a different
            // one.
            let crossed = run_query(&format!(
                r#"{{ mimeRows(after: "{cursor}",
                     orderBy: [{{ field: MIME_TYPE, direction: ASC }}],
                     filter: {{ mimeType: {{ startsWith: "image" }} }}) {{ total }} }}"#
            ))
            .await;
            assert!(
                !crossed.errors.is_empty(),
                "a cursor is bound to its filter"
            );
        })
        .await;
    }

    /// The profiles come back with the rules each one waives, not a count of
    /// them, and the listing filters, orders and pages like every other.
    #[tokio::test]
    async fn the_yolo_profiles_report_what_they_waive() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let path = crate::yolo::yolo_path();
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
            std::fs::write(&path, crate::commands::yolo::EXAMPLE_TOML).expect("the profiles");

            let answer = run_query(
                "{ yoloProfiles(first: 10) { total results
                     { id name default questions checkpoints gate
                       toolRules { allow ask deny }
                       shellRules { allow { command args } ask { command } deny { command } } } } }",
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            assert_eq!(json["yoloProfiles"]["total"], 2);
            let careful = json["yoloProfiles"]["results"]
                .as_array()
                .expect("the profiles")
                .iter()
                .find(|profile| profile["name"] == "careful")
                .expect("the example's first profile")
                .clone();
            assert_eq!(careful["id"], "yoloProfile:careful");
            assert_eq!(careful["default"], "ASK");
            assert_eq!(careful["questions"], "ASK");
            assert_eq!(careful["gate"], "AUTO");
            assert_eq!(careful["toolRules"]["allow"][0], "@builtin");
            assert_eq!(careful["toolRules"]["ask"][0], "web_fetch");
            assert_eq!(
                careful["toolRules"]["deny"].as_array().map(Vec::len),
                Some(0)
            );
            let allowed = careful["shellRules"]["allow"]
                .as_array()
                .expect("the allow rules");
            assert_eq!(allowed[0]["command"], "cargo *");
            assert!(allowed[0]["args"].is_null(), "no args means any: {allowed:?}");
            assert_eq!(allowed[2]["args"][0], "target/**");
            assert_eq!(careful["shellRules"]["deny"][0]["command"], "curl");

            // Filtered, ordered and paged the way every other listing is.
            let paged = run_query(
                "{ yoloProfiles(filter: { name: { contains: \"l\" } },
                     orderBy: [{ field: NAME, direction: ASC }], first: 1)
                     { total cursor results { name } } }",
            )
            .await;
            assert!(paged.errors.is_empty(), "{:?}", paged.errors);
            let json = serde_json::to_value(&paged.data).expect("data serializes");
            assert_eq!(json["yoloProfiles"]["total"], 2);
            assert_eq!(json["yoloProfiles"]["results"][0]["name"], "build-only");
            let cursor = json["yoloProfiles"]["cursor"]
                .as_str()
                .expect("a second page")
                .to_string();
            let rest = run_query(&format!(
                "{{ yoloProfiles(filter: {{ name: {{ contains: \"l\" }} }},
                     orderBy: [{{ field: NAME, direction: ASC }}], first: 1,
                     after: \"{cursor}\") {{ results {{ name }} }} }}"
            ))
            .await;
            assert!(rest.errors.is_empty(), "{:?}", rest.errors);
            let json = serde_json::to_value(&rest.data).expect("data serializes");
            assert_eq!(json["yoloProfiles"]["results"][0]["name"], "careful");

            // One by name, and a name the file has no table for.
            let one = run_query(
                "{ yoloProfile(name: \"build-only\") { name toolRules { allow } }
                   ghost: yoloProfile(name: \"nope\") { name } }",
            )
            .await;
            assert!(one.errors.is_empty(), "{:?}", one.errors);
            let json = serde_json::to_value(&one.data).expect("data serializes");
            assert_eq!(json["yoloProfile"]["toolRules"]["allow"][0], "read_file");
            assert!(json["ghost"].is_null(), "a name with no table is null");
        })
        .await;
    }

    /// The config's gateways and limits come through, with no secret in them.
    #[tokio::test]
    async fn the_config_carries_its_gateways_and_limits() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let path = home.join("config.toml");
            std::fs::write(
                &path,
                "default_provider = \"local\"\n\
                 \n[model_providers.local]\nbase_url = \"http://127.0.0.1:11434/v1\"\n\
                 api_key = \"sk-secret\"\nmodels = [\"llama\"]\n",
            )
            .expect("a config file");
            let state = crate::commands::serve::testutil::state_with_config_at(&path);
            let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
                .data(state)
                .finish();
            let answer = schema
                .execute(Request::new(
                    "{ config { routing { defaultProvider } blueprintPaths mcpServerCount
                         server { apiVersion capabilities isAdminEnabled
                           limits { maxPageSize maxIds maxFileBytes maxListingEntries
                             maxSearchScan maxHistoryLimit maxConcurrentRequests
                             maxUploadBytes requestTimeoutSecs } }
                         gateways { name kind baseUrl script hasApiKey headerNames models
                           unknownKeys } } }",
                ))
                .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let config = &json["config"];
            assert_eq!(config["routing"]["defaultProvider"], "local");
            let gateway = &config["gateways"][0];
            assert_eq!(gateway["name"], "local");
            assert_eq!(gateway["kind"], "SCRIPT", "the entry names no kind");
            assert_eq!(gateway["baseUrl"], "http://127.0.0.1:11434/v1");
            assert_eq!(gateway["models"], serde_json::json!(["llama"]));
            // The key is a boolean and never a value: a console needs to know
            // whether one is configured and never needs the key itself.
            assert_eq!(gateway["hasApiKey"], true);
            let rendered = serde_json::to_string(config).expect("it serializes");
            assert!(!rendered.contains("sk-secret"), "no secret travels");
            assert!(
                config["server"]["limits"]["maxPageSize"]
                    .as_i64()
                    .is_some_and(|n| n > 0)
            );
            assert!(
                config["server"]["capabilities"]
                    .as_array()
                    .expect("capabilities")
                    .iter()
                    .any(|name| name == "graphql"),
                "the server says what it can do"
            );
        })
        .await;
    }

    /// The approval inbox needs the daemon, so silence from it is a refusal
    /// rather than an empty inbox.
    #[tokio::test]
    async fn the_approval_inbox_needs_the_daemon() {
        let answer = run_query("{ openInteractions { results { id prompt } } }").await;
        assert_eq!(
            answer
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"DAEMON_UNAVAILABLE\"".to_string())
        );
    }

    /// A search names where to look, and the scopes travel into the cursor's
    /// digest so a cursor cannot be carried to a different search.
    #[tokio::test]
    async fn a_search_can_name_where_to_look() {
        crate::runstate::with_isolated_runs_dir_async("graphql-scopes", |_dir| async move {
            let mut meta = meta_at("searchable", 100);
            meta.task = "find the parser bug".to_string();
            create_run(&meta).expect("run written");

            let answer = run_query(
                r#"{ runs(search: { query: "parser", in: [META, LOGS, CONTEXT, JOURNAL, FILES] })
                     { results { id } highlights { runId field snippet stageIndex } } }"#,
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            assert_eq!(ids_of(&answer.data, "runs"), vec!["searchable".to_string()]);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let highlights = json["runs"]["highlights"].as_array().expect("highlights");
            assert!(
                highlights.iter().all(|hit| hit["runId"] == "searchable"),
                "each match names its run: {highlights:?}"
            );
            assert!(
                highlights.iter().any(|hit| hit["snippet"]
                    .as_str()
                    .is_some_and(|s| s.contains("parser"))),
                "the match says where it was: {highlights:?}"
            );
        })
        .await;
    }
}

/// The answers that only appear when something is wrong or unusual.
mod the_awkward_shapes {
    use super::*;

    /// A config that will not load is reported inside a healthy answer, with
    /// where and why.
    ///
    /// The alternative is refusing the request, which would leave a console with
    /// nothing to render and no way to say what is wrong: the settings screen is
    /// exactly where somebody would fix it.
    #[tokio::test]
    async fn a_config_that_will_not_load_is_reported_not_refused() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let path = home.join("config.toml");
            std::fs::write(&path, "default_provider = \"openai\"\n").expect("a config file");
            let state = crate::commands::serve::testutil::state_with_config_at(&path);
            // Broken after the server started, which is the order a person
            // editing it produces: the server keeps the last one that loaded and
            // says what is wrong with the file on disk.
            //
            // The modified time is pushed forward, because that is what the
            // reloader compares. Two writes inside one tick of the filesystem's
            // clock look like no change at all, and the coarser that clock is the
            // more often the test says the file still loads.
            std::fs::write(&path, "default_provider = \n").expect("a broken config file");
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("the file to stamp")
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
                .expect("a later modified time");
            let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
                .data(state)
                .finish();
            let answer = schema
                .execute(Request::new(
                    "{ config { routing { defaultProvider } health { savedAt
                         error { kind path message line column key since note } } } }",
                ))
                .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let error = &json["config"]["health"]["error"];
            assert_eq!(error["kind"], "PARSE");
            assert!(
                error["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("config.toml")),
                "it names the file: {error}"
            );
            assert!(
                error["message"].as_str().is_some_and(|m| !m.is_empty()),
                "and says what is wrong: {error}"
            );
            // When this server first saw it in this state. A banner that has
            // been up an hour is a different thing from one that just appeared.
            assert!(
                error["since"].as_i64().is_some_and(|at| at > 0),
                "and when it started: {error}"
            );
            assert!(
                error["line"].as_i64().is_some(),
                "a parse failure knows where it is: {error}"
            );
            assert!(
                error["note"].as_str().is_some_and(|n| !n.is_empty()),
                "said in words for a client that renders strings: {error}"
            );
            // The config in force is still answered: it is the last one that
            // loaded, which is what the daemon is running on.
            assert!(
                json["config"]["routing"]["defaultProvider"]
                    .as_str()
                    .is_some()
            );
        })
        .await;
    }

    /// An agent's own scripts and paths come back beside the global ones.
    #[tokio::test]
    async fn an_agents_own_tools_carry_its_name_and_path() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let agent = home.join(".leviath").join("agents").join("coder");
            std::fs::create_dir_all(agent.join("tools")).expect("the agent's tools directory");
            std::fs::write(
                agent.join("agent.leviath"),
                "[agent]\nname = \"coder\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
                 \n[context.regions.work]\nkind = \"temporary\"\nmax_tokens = 100\n\
                 \n[stages.only]\nmode = \"autonomous\"\n",
            )
            .expect("a manifest");
            std::fs::write(
                agent.join("tools").join("summarize.rhai"),
                "// @tool summarize\n// @description sums up\n\"ok\"",
            )
            .expect("a tool");

            let answer = run_query(
                r#"{ blueprint(name: "coder") { tools(first: 200) { results { name origin
                     ... on ScriptToolOutput { path blueprint requires } } } } }"#,
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let tools = json["blueprint"]["tools"]["results"]
                .as_array()
                .expect("the tools");
            let own = tools
                .iter()
                .find(|tool| tool["name"] == "summarize")
                .expect("the agent's own tool");
            assert_eq!(own["blueprint"], "coder", "whose tool it is");
            assert!(
                own["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("summarize.rhai")),
                "and where it came from: {own}"
            );
        })
        .await;
    }

    /// A profile that asks before everything reads as `ask` rather than as the
    /// other word.
    #[tokio::test]
    async fn a_profile_that_asks_says_ask() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let path = crate::yolo::yolo_path();
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
            std::fs::write(
                &path,
                "[cautious]\ndefault = \"ask\"\nquestions = \"ask\"\n\
                 checkpoints = \"ask\"\ngate = \"ask\"\n",
            )
            .expect("the profiles");

            let answer = run_query(
                "{ yoloProfiles { results { name default questions checkpoints gate } } }",
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let profile = &json["yoloProfiles"]["results"][0];
            assert_eq!(profile["default"], "ASK");
            assert_eq!(profile["questions"], "ASK");
            assert_eq!(profile["checkpoints"], "ASK");
            assert_eq!(profile["gate"], "ASK");
        })
        .await;
    }

    /// A search that hits a stage's own text says which stage it was.
    #[tokio::test]
    async fn a_match_inside_a_stage_names_the_stage() {
        crate::runstate::with_isolated_runs_dir_async("graphql-stage-hit", |_dir| async move {
            let meta = meta_at("logged", 100);
            create_run(&meta).expect("run written");
            // The search reads the stages the ledger records rather than the
            // directories that happen to exist, so the ledger comes first.
            crate::runstate::write_stages_index(
                "logged",
                &[leviath_core::run_meta::StageRecord::new(
                    "only".to_string(),
                    0,
                )],
            )
            .expect("the ledger");
            crate::runstate::append_stage_output("logged", 0, "the parser gave up\n");

            let answer = run_query(
                r#"{ runs(search: { query: "parser", in: [LOGS] })
                     { highlights { field snippet stageIndex } } }"#,
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let highlights = json["runs"]["highlights"].as_array().expect("highlights");
            let hit = highlights
                .iter()
                .find(|hit| hit["stageIndex"].as_i64().is_some())
                .expect("a match that knows its stage");
            assert_eq!(hit["stageIndex"], 0, "{hit}");
        })
        .await;
    }
}

/// A config file that will not parse is reported, not read as an empty machine.
///
/// "No MCP servers" and "this file is broken" lead somewhere different, and a
/// console that showed the first for the second would hide the reason its tools
/// disappeared. The same holds for the script registry, which reads the same
/// file.
#[tokio::test]
async fn an_unparseable_config_is_reported_rather_than_read_as_empty() {
    crate::config::with_isolated_config_path_async("graphql-bad-config", |dir| async move {
        std::fs::write(dir.join("config.toml"), "this is not = = toml").expect("a broken config");

        let answer = run_query("{ mcpServers { results { name } } }").await;
        let message = &answer
            .errors
            .first()
            .expect("a broken config is an error")
            .message;
        assert!(!message.is_empty(), "it says what went wrong");
    })
    .await;
}

/// A blueprint name that is not a name is refused before any directory is read.
///
/// The name becomes a path, so this is the check that stops one being used as a
/// way out of the agents directory. An empty list would look like an answer.
#[tokio::test]
async fn scripts_of_an_unsafe_blueprint_name_are_refused() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let dir = home.join(".leviath").join("agents").join("sneaky");
        std::fs::create_dir_all(&dir).expect("the agent directory");
        std::fs::write(
            dir.join(leviath_core::files::MANIFEST_FILENAME),
            manifest_text("../../etc", "1.0.0"),
        )
        .expect("a manifest");

        let answer =
            run_query(r#"{ blueprint(name: "../../etc") { scripts { results { name } } } }"#).await;
        let message = &answer.errors.first().expect("a refusal").message;
        assert!(message.contains("Invalid agent name"), "{message}");
    })
    .await;
}

/// A listing cursor from another query is refused rather than resumed.
#[tokio::test]
async fn a_listing_cursor_from_elsewhere_is_refused() {
    let answer = run_query(r#"{ runs(after: "not-a-cursor") { total } }"#).await;
    let message = &answer.errors.first().expect("a refusal").message;
    assert!(!message.is_empty(), "{message}");
}

/// Oldest-first paging mints cursors of its own order.
///
/// The order is part of the cursor, so a page taken one way cannot be resumed as
/// though it were the other. That is what stops a client walking a listing in
/// both directions and seeing a run once or twice by accident.
#[tokio::test]
async fn an_ascending_listing_mints_its_own_cursors() {
    crate::runstate::with_isolated_runs_dir_async("graphql-asc-cursors", |_dir| async move {
        for (id, at) in [("first", 100), ("second", 200), ("third", 300)] {
            create_run(&meta_at(id, at)).expect("run written");
        }
        let answer = run_query(
            "{ runs(first: 2, orderBy: [{ field: STARTED_AT, direction: ASC }]) {
                 cursor
                 results { id }
               } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let page = &json["runs"];
        assert_eq!(page["results"][0]["id"], "first", "oldest first");

        // The cursor resumes the same order rather than starting again.
        let cursor = page["cursor"].as_str().expect("a cursor");
        let answer = run_query(&format!(
            r#"{{ runs(first: 2, after: "{cursor}",
                       orderBy: [{{ field: STARTED_AT, direction: ASC }}]) {{
                 results {{ id }} }} }}"#
        ))
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["results"][0]["id"], "third");

        // A cursor minted for one order does not resume another.
        let crossed = run_query(&format!(
            r#"{{ runs(first: 2, after: "{cursor}") {{ results {{ id }} }} }}"#
        ))
        .await;
        assert!(
            crossed
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("order="),
            "{:?}",
            crossed.errors
        );
    })
    .await;
}

/// Both values a profile's default can be, and both a human knob can be.
///
/// Two waivers, and neither is a bare on or off: a profile waives a prompt, it
/// never adds a refusal, so there is no third value for denying anything.
#[test]
fn a_profiles_default_is_one_of_two_waivers() {
    use super::super::types::machine::{YoloHuman, YoloWaiver, human_of, waiver_of};
    assert_eq!(
        waiver_of(crate::yolo::rules::Waiver::Allow),
        YoloWaiver::Allow
    );
    assert_eq!(waiver_of(crate::yolo::rules::Waiver::Ask), YoloWaiver::Ask);
    assert_eq!(human_of(crate::yolo::rules::Human::Ask), YoloHuman::Ask);
    assert_eq!(human_of(crate::yolo::rules::Human::Auto), YoloHuman::Auto);
}

/// The configured blueprint directories come back as text.
///
/// A path is not a string on every platform, so this is where one becomes one.
/// A console lists these to say where a blueprint would be installed.
#[tokio::test]
async fn the_config_lists_where_blueprints_are_looked_for() {
    let state = crate::commands::serve::testutil::state_with_agent_paths(vec![
        std::path::PathBuf::from("/srv/agents"),
        std::path::PathBuf::from("/opt/more-agents"),
    ]);
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    let answer = schema
        .execute(Request::new("{ config { blueprintPaths } }"))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    let paths: Vec<&str> = json["config"]["blueprintPaths"]
        .as_array()
        .expect("the paths")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(paths, vec!["/srv/agents", "/opt/more-agents"]);
}

/// A manifest for the checks to look at.
fn manifest_text(name: &str, version: &str) -> String {
    format!(
        "[agent]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"d\"\n\n\
         [stages.only]\nmode = \"autonomous\"\n"
    )
}

// ─── The four pure checks ─────────────────────────────────────────────────────
//
// Query fields, not mutations: text in, verdict out. Each one sits beside the
// write it precedes, which is where it appears in a form, not what it does.

/// Validation reports what it found. A manifest that will not install is a
/// report with the reasons, not a failed request.
#[tokio::test]
async fn validation_reports_rather_than_fails() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let good = manifest_text("checked", "1.0.0")
            .replace('\n', "\\n")
            .replace('"', "\\\"");
        let answer = run_query(&format!(
            r#"query {{ validateBlueprint(manifest: "{good}") {{ valid errors warnings }} }}"#
        ))
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["validateBlueprint"]["valid"], true);
        assert_eq!(
            json["validateBlueprint"]["errors"].as_array().map(Vec::len),
            Some(0)
        );

        let bad = run_query(
            r#"query { validateBlueprint(manifest: "not a manifest") { valid errors } }"#,
        )
        .await;
        assert!(bad.errors.is_empty(), "a finding is not a request failure");
        let json = serde_json::to_value(&bad.data).expect("data serializes");
        assert_eq!(json["validateBlueprint"]["valid"], false);
        assert!(
            !json["validateBlueprint"]["errors"]
                .as_array()
                .expect("errors")
                .is_empty(),
            "it says why"
        );
    })
    .await;
}
/// The three checks that answer without changing anything.
///
/// Each is a mutation because it belongs beside the write it precedes, and none
/// of them is gated: nothing is dialled, nothing is written, and a form that
/// checks as somebody types should not need `--allow-admin`.
#[tokio::test]
async fn the_checks_that_change_nothing_need_no_flag() {
    let good = run_query(
        r#"query { validateProviderKey(provider: "anthropic", key: "sk-ant-abc")
             { valid message } }"#,
    )
    .await;
    assert!(good.errors.is_empty(), "{:?}", good.errors);
    let json = serde_json::to_value(&good.data).expect("data serializes");
    assert_eq!(json["validateProviderKey"]["valid"], true);
    assert!(json["validateProviderKey"]["message"].is_null());

    let wrong = run_query(
        r#"query { validateProviderKey(provider: "anthropic", key: "nope") { valid message } }"#,
    )
    .await;
    let json = serde_json::to_value(&wrong.data).expect("data serializes");
    assert_eq!(json["validateProviderKey"]["valid"], false);
    assert!(
        json["validateProviderKey"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("sk-ant-")),
        "it says what the format is"
    );

    // The address is judged before the key: a key cannot be judged beyond being
    // present until there is somewhere to send it.
    let bad_url = run_query(
        r#"query { validateProviderKey(provider: "anthropic", key: "sk-ant-abc",
             baseUrl: "not a url") { valid message } }"#,
    )
    .await;
    let json = serde_json::to_value(&bad_url.data).expect("data serializes");
    assert_eq!(json["validateProviderKey"]["valid"], false);

    let compiles = run_query(
        r#"query { validateScript(kind: TOOL,
             content: "// @tool summarize\n// @description sums up\n\"ok\"")
             { valid error } }"#,
    )
    .await;
    assert!(compiles.errors.is_empty(), "{:?}", compiles.errors);
    let json = serde_json::to_value(&compiles.data).expect("data serializes");
    assert_eq!(json["validateScript"]["valid"], true);

    let broken =
        run_query(r#"query { validateScript(kind: TOOL, content: "fn (") { valid error } }"#).await;
    let json = serde_json::to_value(&broken.data).expect("data serializes");
    assert_eq!(json["validateScript"]["valid"], false);
    assert!(json["validateScript"]["error"].as_str().is_some());

    // A registry no compiler claims is a refusal rather than a verdict. The
    // listing reports a file nothing has claimed as `CANDIDATE`, and there is
    // nothing to have an opinion about it.
    let unknown =
        run_query(r#"query { validateScript(kind: CANDIDATE, content: "") { valid } }"#).await;
    assert_eq!(
        unknown
            .errors
            .first()
            .expect("a refusal")
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .map(ToString::to_string),
        Some("\"BAD_USER_INPUT\"".to_string())
    );
}

/// A yolo profile's decision about one call, through the same code path the
/// command uses.
#[tokio::test]
async fn a_yolo_profile_decides_about_one_call() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let path = crate::yolo::yolo_path();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, crate::commands::yolo::EXAMPLE_TOML).expect("the profiles");

        let decided = run_query(
            "{ yoloProfile(name: \"careful\") {
                 read: decide(tool: \"read_file\", kind: BUILTIN)
                     { tool configured policy reason }
                 refused: decide(tool: \"shell\", kind: BUILTIN,
                     args: { command: \"curl https://example.com\" })
                     { policy reason }
                 asked: decide(tool: \"web_fetch\", kind: BUILTIN,
                     args: { url: \"https://example.com\" }) { policy }
                 subagent: decide(tool: \"run_researcher\", kind: SUBAGENT) { policy }
                 script: decide(tool: \"summarise\", kind: SCRIPT) { policy }
                 mcp: decide(tool: \"docs__search\", kind: MCP) { policy } } }",
        )
        .await;
        assert!(decided.errors.is_empty(), "{:?}", decided.errors);
        let json = serde_json::to_value(&decided.data).expect("data serializes");
        let profile = &json["yoloProfile"];
        assert_eq!(profile["read"]["tool"], "read_file");
        assert_eq!(profile["read"]["policy"], "ALLOW");
        assert_eq!(profile["refused"]["policy"], "DENY");
        assert_eq!(profile["refused"]["reason"], "shell deny rule \"curl\"");
        assert_eq!(profile["asked"]["policy"], "ASK");
        // Every kind decides: `careful` allows `@builtin` and nothing else,
        // so the three other sources fall to its `ask` default.
        assert_eq!(profile["subagent"]["policy"], "ASK");
        assert_eq!(profile["script"]["policy"], "ASK");
        assert_eq!(profile["mcp"]["policy"], "ASK");
        assert!(
            profile["read"]["configured"] == "ALLOW" || profile["read"]["configured"] == "ASK",
            "the configured half is what the config layers resolved: {profile}"
        );
    })
    .await;
}

/// A manifest that will not parse is a report rather than a written blueprint.
#[tokio::test]
async fn a_blueprint_that_will_not_parse_is_reported_not_written() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let report = run_query(
            r#"query { validateBlueprint(manifest: "[agent]\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\nentry_stage = \"nope\"\n\n[stages.only]\nmode = \"autonomous\"\n")
                 { valid errors warnings } }"#,
        )
        .await;
        assert!(report.errors.is_empty(), "a finding is not a failure");
        let json = serde_json::to_value(&report.data).expect("data serializes");
        assert_eq!(json["validateBlueprint"]["valid"], false);
        assert!(
            !json["validateBlueprint"]["errors"]
                .as_array()
                .expect("errors")
                .is_empty(),
            "it says what is missing"
        );
    })
    .await;
}

/// A blueprint validated against an agent directory names the agent, and a name
/// that could leave that directory is refused before any path is built.
#[tokio::test]
async fn validating_against_an_agent_refuses_a_name_that_could_escape() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            r#"query { validateBlueprint(manifest: "[agent]\nname = \"x\"\n",
                 as: { name: "../elsewhere" }) { valid } }"#,
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string()),
            "{}",
            error.message
        );
    })
    .await;
}

// ─── the daemon's journal ───────────────────────────────────────────────────

/// Run one query against a schema wired to the given daemon.
async fn query_daemon(
    control: leviath_runtime::control_socket::ControlClient,
    query: &str,
) -> async_graphql::Response {
    let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    state.control = control;
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

const JOURNAL_QUERY: &str = "{ daemon { journal { healthy appendsAttempted appendsFailed \
     snapshotsFailed queueDepth lastError { runId path message at } } } }";

/// The whole point of the field: a daemon that has lost a record says so, names
/// the run and the file, and stops reading as healthy.
#[tokio::test]
async fn the_journal_field_reports_a_daemon_that_has_lost_a_record() {
    let (control, _dir, _srv) = crate::commands::serve::testutil::fake_daemon(|_| {
        leviath_runtime::control_socket::ControlResponse::List {
            runs: vec![],
            finished: vec![],
            health: Box::new(leviath_runtime::host::DaemonHealth {
                journal: leviath_runtime::persist_stats::JournalHealth {
                    appends_attempted: 12,
                    appends_failed: 3,
                    snapshots_failed: 1,
                    queue_depth: 4,
                    last_error: Some(leviath_runtime::persist_stats::JournalError {
                        run_id: "run-a".to_string(),
                        path: "/runs/run-a/run.lvr".to_string(),
                        message: "Permission denied".to_string(),
                        at: 1_700_000_000,
                    }),
                },
                ..Default::default()
            }),
        }
    });

    let answer = query_daemon(control, JOURNAL_QUERY).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["daemon"]["journal"]["healthy"], false);
    assert_eq!(json["daemon"]["journal"]["appendsAttempted"], 12);
    assert_eq!(json["daemon"]["journal"]["appendsFailed"], 3);
    assert_eq!(json["daemon"]["journal"]["snapshotsFailed"], 1);
    assert_eq!(json["daemon"]["journal"]["queueDepth"], 4);
    assert_eq!(json["daemon"]["journal"]["lastError"]["runId"], "run-a");
    assert_eq!(
        json["daemon"]["journal"]["lastError"]["path"],
        "/runs/run-a/run.lvr"
    );
    assert_eq!(
        json["daemon"]["journal"]["lastError"]["message"],
        "Permission denied"
    );
    assert_eq!(
        json["daemon"]["journal"]["lastError"]["at"],
        1_700_000_000i64
    );
}

/// A daemon writing everything it is asked to reads as healthy, with nothing to
/// describe.
#[tokio::test]
async fn a_daemon_that_has_lost_nothing_reads_as_healthy() {
    let (control, _dir, _srv) = crate::commands::serve::testutil::fake_daemon(|_| {
        leviath_runtime::control_socket::ControlResponse::List {
            runs: vec![],
            finished: vec![],
            health: Box::default(),
        }
    });

    let answer = query_daemon(control, JOURNAL_QUERY).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["daemon"]["journal"]["healthy"], true);
    assert_eq!(
        json["daemon"]["journal"]["lastError"],
        serde_json::Value::Null
    );
}

/// Null when the daemon cannot be reached, and null when it answers something
/// else: this reading exists only on the daemon, so there is nothing to fall
/// back to.
#[tokio::test]
async fn an_unreachable_daemon_has_no_journal_to_report() {
    let answer = run_query(JOURNAL_QUERY).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["daemon"]["journal"], serde_json::Value::Null);

    let (control, _dir, _srv) = crate::commands::serve::testutil::fake_daemon(|_| {
        leviath_runtime::control_socket::ControlResponse::Ok { ok: true }
    });
    let answer = query_daemon(control, JOURNAL_QUERY).await;
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["daemon"]["journal"], serde_json::Value::Null);
}

// ─── the lookups and the jobs listing ───────────────────────────────────────
//
// Every root listing takes the same five arguments and every `Node` type has a
// typed singular beside it, so what these pin is that the two agree: the id a
// listing hands out is the id the lookup answers to, and a name nothing is
// filed under is an absence rather than a failure.

/// The update runs are a listing, and one job answers to its own id.
///
/// Oldest first, because a job's id carries the second it started: a console
/// reading the history wants them in the order they ran.
#[tokio::test]
async fn the_update_runs_are_paged_and_answer_to_their_ids() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        let job = state.update_jobs.start().expect("nothing else is running");
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();

        let listed = schema
            .execute(Request::new(
                "{ updateJobs { results { id status } cursor total } }",
            ))
            .await;
        assert!(listed.errors.is_empty(), "{:?}", listed.errors);
        let json = serde_json::to_value(&listed.data).expect("data serializes");
        assert_eq!(json["updateJobs"]["total"], 1);
        assert_eq!(json["updateJobs"]["results"][0]["id"], job.id);
        assert!(
            json["updateJobs"]["cursor"].is_null(),
            "one job is one page: {json}"
        );

        // The same listing read the other way round, which is what proves the
        // order is the client's and not the registry's.
        let newest = schema
            .execute(Request::new(
                "{ updateJobs(orderBy: [{ field: ID, direction: DESC }]) { results { id } } }",
            ))
            .await;
        assert!(newest.errors.is_empty(), "{:?}", newest.errors);
        let json = serde_json::to_value(&newest.data).expect("data serializes");
        assert_eq!(json["updateJobs"]["results"][0]["id"], job.id);

        let one = schema
            .execute(Request::new(format!(
                "{{ updateJob(id: \"{}\") {{ id status }} }}",
                job.id
            )))
            .await;
        assert!(one.errors.is_empty(), "{:?}", one.errors);
        let json = serde_json::to_value(&one.data).expect("data serializes");
        assert_eq!(json["updateJob"]["status"], "RUNNING");

        let ghost = schema
            .execute(Request::new(r#"{ updateJob(id: "update-1-1") { id } }"#))
            .await;
        let json = serde_json::to_value(&ghost.data).expect("data serializes");
        assert!(json["updateJob"].is_null(), "a job nothing started");

        // The filter reaches the listing rather than being accepted and
        // ignored.
        let filtered = schema
            .execute(Request::new(format!(
                r#"{{ updateJobs(filter: {{ id: {{ eq: "{}" }} }}) {{ total }} }}"#,
                job.id
            )))
            .await;
        assert!(filtered.errors.is_empty(), "{:?}", filtered.errors);
        let json = serde_json::to_value(&filtered.data).expect("data serializes");
        assert_eq!(json["updateJobs"]["total"], 1);
        let missed = schema
            .execute(Request::new(
                r#"{ updateJobs(filter: { id: { eq: "update-nowhere" } }) { total } }"#,
            ))
            .await;
        let json = serde_json::to_value(&missed.data).expect("data serializes");
        assert_eq!(json["updateJobs"]["total"], 0);

        // A cursor from elsewhere, or simply mangled, is refused rather than
        // silently starting the walk over.
        let mangled = schema
            .execute(
                Request::new("query($after: Cursor) { updateJobs(after: $after) { total } }")
                    .variables(Variables::from_json(serde_json::json!({ "after": "zzz" }))),
            )
            .await;
        assert!(
            mangled
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("Invalid cursor"),
            "{:?}",
            mangled.errors
        );
    })
    .await;
}

/// Every function `#[mirror]` wrote for the export types runs at least once.
///
/// `runExport` and `node` are lookups rather than a listing, so no query ever
/// reaches the generated filter and order code the way a listing's own tests
/// do; this is that type's own measurement, the same as any converted file's.
#[tokio::test]
async fn every_export_mirrored_function_runs() {
    use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum};

    exercise_enum(&[
        super::jobs::ExportStatus::Queued,
        super::jobs::ExportStatus::Complete,
    ])
    .await;

    // Every state the core job can be in reads back as the value that names
    // it, so a client polling an export is never told a state this build
    // invented.
    use crate::commands::serve::core::export::ExportStatus as Core;
    for (core, named) in [
        (Core::Queued, super::jobs::ExportStatus::Queued),
        (Core::Running, super::jobs::ExportStatus::Running),
        (Core::Complete, super::jobs::ExportStatus::Complete),
        (Core::Failed, super::jobs::ExportStatus::Failed),
    ] {
        assert_eq!(super::jobs::ExportStatus::from(core), named);
    }

    let export = super::RunExport {
        id: async_graphql::ID("export-1".to_string()),
        status: super::jobs::ExportStatus::Complete,
        written: 3,
        error: None,
        download_url: Some("https://example/export.jsonl".to_string()),
    };
    exercise(std::slice::from_ref(&export)).await;
}

/// A provider answers to its registry name and to the id it publishes, and a
/// name this build has no provider for is an absence.
#[tokio::test]
async fn a_provider_answers_to_its_name_and_to_its_node_id() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let listed = run_query("{ providers { results { id name display } } }").await;
        assert!(listed.errors.is_empty(), "{:?}", listed.errors);
        let json = serde_json::to_value(&listed.data).expect("data serializes");
        let first = &json["providers"]["results"][0];
        let name = first["name"].as_str().expect("a registry name").to_string();
        let id = first["id"].as_str().expect("a node id").to_string();
        assert_eq!(id, format!("provider:{name}"));

        let one = run_query(&format!("{{ provider(name: \"{name}\") {{ id name }} }}")).await;
        assert!(one.errors.is_empty(), "{:?}", one.errors);
        let json = serde_json::to_value(&one.data).expect("data serializes");
        assert_eq!(json["provider"]["id"], id);

        // The same provider through `node`, which is what makes the id worth
        // publishing rather than a string a client has to take apart.
        let routed = run_query(&format!(
            "{{ node(id: \"{id}\") {{ id ... on ProviderOutput {{ name }} }} }}"
        ))
        .await;
        assert!(routed.errors.is_empty(), "{:?}", routed.errors);
        let json = serde_json::to_value(&routed.data).expect("data serializes");
        assert_eq!(json["node"]["name"], name);

        let ghost = run_query(
            r#"{ provider(name: "nowhere") { id }
                 node(id: "provider:nowhere") { id } }"#,
        )
        .await;
        assert!(ghost.errors.is_empty(), "{:?}", ghost.errors);
        let json = serde_json::to_value(&ghost.data).expect("data serializes");
        assert!(json["provider"].is_null());
        assert!(json["node"].is_null());
    })
    .await;
}

/// The providers listing filters, orders both ways, resumes a cursor, refuses
/// one minted for a different filter, and counts lazily.
#[tokio::test]
async fn the_providers_listing_filters_orders_pages_and_refuses_a_foreign_cursor() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();

        let all = answer(
            &schema,
            "{ providers(orderBy: [{ field: NAME, direction: ASC }]) \
               { results { name } total } }",
        )
        .await;
        let names: Vec<&str> = all["providers"]["results"]
            .as_array()
            .expect("the known providers")
            .iter()
            .map(|provider| provider["name"].as_str().unwrap_or_default())
            .collect();
        assert!(
            names.len() >= 2,
            "more than one provider is known: {names:?}"
        );
        let total = all["providers"]["total"].as_i64().expect("a count");

        // The filter reaches the listing.
        let one_name = names.first().copied().expect("a name");
        let filtered = answer(
            &schema,
            &format!(r#"{{ providers(filter: {{ name: {{ eq: "{one_name}" }} }}) {{ total }} }}"#),
        )
        .await;
        assert_eq!(filtered["providers"]["total"], 1);
        let missed = answer(
            &schema,
            r#"{ providers(filter: { name: { eq: "nobody-registers-this" } }) { total } }"#,
        )
        .await;
        assert_eq!(missed["providers"]["total"], 0);

        // Ordered the other way is the same set, reversed.
        let descending = answer(
            &schema,
            "{ providers(orderBy: [{ field: NAME, direction: DESC }]) { results { name } } }",
        )
        .await;
        let mut desc_names: Vec<&str> = descending["providers"]["results"]
            .as_array()
            .expect("descending")
            .iter()
            .map(|provider| provider["name"].as_str().unwrap_or_default())
            .collect();
        desc_names.reverse();
        assert_eq!(names, desc_names, "the same order, read the other way");

        // A page, then the rest, resumed from the cursor the first page
        // handed back.
        let page = answer(
            &schema,
            "{ providers(first: 1, orderBy: [{ field: NAME, direction: ASC }]) \
               { results { name } cursor } }",
        )
        .await;
        let cursor = page["providers"]["cursor"]
            .as_str()
            .expect("more pages follow")
            .to_string();
        let rest = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { providers(first: 50, after: $after, \
                       orderBy: [{ field: NAME, direction: ASC }]) { results { name } } }",
                )
                .variables(Variables::from_json(
                    serde_json::json!({ "after": cursor.clone() }),
                )),
            )
            .await;
        assert!(rest.errors.is_empty(), "{:?}", rest.errors);
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        let resumed: Vec<&str> = json["providers"]["results"]
            .as_array()
            .expect("the rest")
            .iter()
            .map(|provider| provider["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(resumed, &names[1..], "no repeat and nothing skipped");

        // A cursor minted under one filter is refused under another: the walk
        // it names is not the walk being asked for.
        let crossed = schema
            .execute(
                Request::new(
                    r#"query($after: Cursor) { providers(after: $after,
                         orderBy: [{ field: NAME, direction: ASC }],
                         filter: { name: { startsWith: "a" } }) { total } }"#,
                )
                .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(
            !crossed.errors.is_empty(),
            "a cursor is bound to its filter"
        );
        assert_eq!(total, i64::try_from(names.len()).unwrap_or(0));
    })
    .await;
}

/// A machine whose config names a provider with a fixed catalogue, for as long
/// as `f` runs.
///
/// The directory outlives the call because the config is read back from it: a
/// reloader whose file has gone answers from the defaults, and the catalogue
/// would then be whatever the machine running the test has keys for.
async fn with_static_models<F, Fut>(f: F)
where
    F: FnOnce(super::super::super::types::AppState) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    crate::commands::serve::testutil::with_home(|home| async move {
        let path = home.join("config.toml");
        // The key is what puts meshy in this install, and meshy is the one
        // built-in provider whose catalogue is a fixed list rather than a
        // request. The config carries it so this test says what it needs,
        // rather than passing only on a machine that has the variable set.
        std::fs::write(
            &path,
            "default_provider = \"openai\"\n\n[providers]\n\
             meshy_api_key = \"msy_not_a_real_key\"\n",
        )
        .expect("a config file");
        f(crate::commands::serve::testutil::state_with_config_at(
            &path,
        ))
        .await;
    })
    .await;
}

/// The models listing filters, orders both ways, pages with a resumable
/// cursor, refuses a cursor from elsewhere, and counts lazily; the lookup and
/// `node` both answer to the id the listing itself hands out.
#[tokio::test]
async fn the_models_listing_filters_orders_pages_and_answers_to_its_id() {
    with_static_models(|state| async move {
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();

        let all = answer(
            &schema,
            "{ models { results { id modelId providerName } total } }",
        )
        .await;
        let models = all["models"]["results"].as_array().expect("static models");
        assert!(
            models.len() >= 2,
            "a built-in provider's catalogue is more than one model: {models:?}"
        );
        assert!(
            models.iter().all(|model| model["providerName"] == "meshy"),
            "only the provider whose catalogue needs no network answers here: {models:?}"
        );
        let total = all["models"]["total"].as_i64().expect("a count");
        assert_eq!(total, i64::try_from(models.len()).unwrap_or(0));

        // The filter reaches the listing rather than being accepted and ignored.
        let filtered = answer(
            &schema,
            r#"{ models(filter: { providerName: { eq: "meshy" } }) { total } }"#,
        )
        .await;
        assert_eq!(filtered["models"]["total"], total);
        let missed = answer(
            &schema,
            r#"{ models(filter: { providerName: { eq: "nobody" } }) { total } }"#,
        )
        .await;
        assert_eq!(missed["models"]["total"], 0);

        // Both directions of the order, proven against each other rather than
        // against a fixed expectation: reversing one reverses the other.
        let ascending = answer(
            &schema,
            "{ models(orderBy: [{ field: ID, direction: ASC }]) { results { id } } }",
        )
        .await;
        let descending = answer(
            &schema,
            "{ models(orderBy: [{ field: ID, direction: DESC }]) { results { id } } }",
        )
        .await;
        let asc_ids: Vec<&str> = ascending["models"]["results"]
            .as_array()
            .expect("ascending")
            .iter()
            .map(|model| model["id"].as_str().unwrap_or_default())
            .collect();
        let mut desc_ids: Vec<&str> = descending["models"]["results"]
            .as_array()
            .expect("descending")
            .iter()
            .map(|model| model["id"].as_str().unwrap_or_default())
            .collect();
        desc_ids.reverse();
        assert_eq!(asc_ids, desc_ids, "the same order, read the other way");

        // A page, then the rest, resumed from the cursor the first page handed
        // back.
        let first_id = asc_ids.first().copied().expect("at least one model");
        let page = answer(
            &schema,
            "{ models(first: 1, orderBy: [{ field: ID, direction: ASC }]) { \
                 results { id } cursor } }",
        )
        .await;
        assert_eq!(page["models"]["results"][0]["id"], first_id);
        let cursor = page["models"]["cursor"]
            .as_str()
            .expect("more pages follow")
            .to_string();
        let rest = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { models(first: 50, after: $after, \
                       orderBy: [{ field: ID, direction: ASC }]) { results { id } } }",
                )
                .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(rest.errors.is_empty(), "{:?}", rest.errors);
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        let resumed: Vec<&str> = json["models"]["results"]
            .as_array()
            .expect("the rest")
            .iter()
            .map(|model| model["id"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(resumed, &asc_ids[1..], "no repeat and nothing skipped");

        // A cursor minted under a different order is refused rather than
        // silently resumed under this one.
        let crossed = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { models(after: $after, \
                       orderBy: [{ field: ID, direction: DESC }]) { total } }",
                )
                .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(!crossed.errors.is_empty(), "a cursor is bound to its order");

        // The listing and the lookup agree on the id.
        let one = answer(
            &schema,
            &format!(r#"{{ model(id: "{first_id}") {{ id modelId }} }}"#),
        )
        .await;
        assert_eq!(one["model"]["id"], first_id);
        let routed = answer(
            &schema,
            &format!(r#"{{ node(id: "{first_id}") {{ id ... on ModelOutput {{ modelId }} }} }}"#),
        )
        .await;
        assert_eq!(routed["node"]["id"], first_id);
    })
    .await;
}

/// A model id nothing here serves is an absence, whichever way it is asked for.
///
/// A machine with no provider configured routes to no model at all, which is
/// exactly the case a client hitting a stale cached id runs into.
#[tokio::test]
async fn a_model_nothing_serves_answers_null() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            r#"{ model(id: "model:openai/gpt-5.6") { id modelId }
                 node(id: "model:openai/gpt-5.6") { id } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert!(json["model"].is_null());
        assert!(json["node"].is_null());
    })
    .await;
}

/// A blueprint's own scripts come back beside the global ones, through the
/// relation rather than through an argument on the root listing.
#[tokio::test]
async fn a_blueprints_scripts_carry_its_own_directory() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let agent = home.join(".leviath").join("agents").join("coder");
        std::fs::create_dir_all(agent.join("tools")).expect("the agent's tools directory");
        std::fs::write(
            agent.join(leviath_core::files::MANIFEST_FILENAME),
            manifest_text("coder", "1.0.0"),
        )
        .expect("a manifest");
        std::fs::write(
            agent.join("tools").join("summarize.rhai"),
            "// @tool summarize\n// @description sums up\n\"ok\"",
        )
        .expect("a tool");

        let answer = run_query(
            r#"{ blueprint(name: "coder") {
                 scripts(filter: { scope: { eq: BLUEPRINT } })
                   { results { name scope blueprintName } total } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let scripts = json["blueprint"]["scripts"]["results"]
            .as_array()
            .expect("the scripts");
        assert!(
            scripts
                .iter()
                .any(|script| script["name"] == "summarize" && script["blueprintName"] == "coder"),
            "its own tool is there, and says whose it is: {scripts:?}"
        );
        assert!(
            scripts.iter().all(|script| script["scope"] == "BLUEPRINT"),
            "the filter reaches the listing: {scripts:?}"
        );
    })
    .await;
}

/// The read-side argument bags read back from their own value, and refuse a
/// field of the wrong type.
///
/// An input type is written for one direction and generated for both, and only
/// a field that will not read walks the half a valid request never does.
#[test]
fn every_read_side_input_round_trips() {
    use super::super::filter::testkit::round_trip;
    use super::runs::{RunSearchOptions, SearchScope};

    round_trip(&RunSearchOptions {
        query: "timeout".to_string(),
        within: Some(vec![SearchScope::Meta, SearchScope::Logs]),
    });
}

/// The approval inbox: every open ask on the machine, read from the daemon's
/// own memory rather than from the run store.
///
/// The run each ask is parked on is a file-backed field, and an ask parked on
/// a run whose record has gone says so rather than answering with a run that
/// is not there.
#[tokio::test]
async fn the_open_interactions_are_the_approval_inbox() {
    use crate::commands::serve::testutil::fake_daemon;
    use leviath_runtime::control_socket::ControlResponse;

    let ask = |id: &str| leviath_core::interaction::InteractionRequest {
        id: id.to_string(),
        kind: leviath_core::interaction::InteractionKind::ToolApproval,
        prompt: "may I?".to_string(),
        options: Vec::new(),
        tool_name: Some("shell".to_string()),
        tool_arguments: None,
        required: true,
        stage_name: "build".to_string(),
        body: None,
        body_format: Default::default(),
    };

    crate::runstate::with_isolated_runs_dir_async("graphql-open-inbox", |_d| async move {
        create_run(&meta_at("parked", 100)).expect("the run is written");

        let (control, _socket, _srv) = fake_daemon(move |_| ControlResponse::Interactions {
            interactions: vec![
                ("parked".to_string(), ask("ask-1")),
                ("gone".to_string(), ask("ask-2")),
            ],
        });
        let answer = run_query_with_daemon(
            control,
            "{ openInteractions(first: 10) { total results { id stageName toolName } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["openInteractions"]["total"], 2);
        assert_eq!(json["openInteractions"]["results"][0]["id"], "ask-1");
        assert_eq!(json["openInteractions"]["results"][0]["toolName"], "shell");

        // The run behind an ask is read from disk, and an ask parked on a run
        // whose record has gone says so.
        let (control, _socket, _srv) = fake_daemon(move |_| ControlResponse::Interactions {
            interactions: vec![("gone".to_string(), ask("ask-2"))],
        });
        let missing = run_query_with_daemon(
            control,
            "{ openInteractions { results { id run { id } } } }",
        )
        .await;
        assert!(
            missing
                .errors
                .first()
                .is_some_and(|error| error.message.contains("not found")),
            "{:?}",
            missing.errors
        );

        // And one whose record is there answers with the run.
        let (control, _socket, _srv) = fake_daemon(move |_| ControlResponse::Interactions {
            interactions: vec![("parked".to_string(), ask("ask-1"))],
        });
        let found =
            run_query_with_daemon(control, "{ openInteractions { results { run { id } } } }").await;
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let json = serde_json::to_value(&found.data).expect("data serializes");
        assert_eq!(
            json["openInteractions"]["results"][0]["run"]["id"],
            "parked"
        );
    })
    .await;
}

/// The approval inbox refuses a page bigger than its cap and a filter too deep
/// to walk, before it asks the daemon anything.
#[tokio::test]
async fn the_open_interactions_refuse_an_oversized_page_and_a_deep_filter() {
    use crate::commands::serve::testutil::fake_daemon;
    use leviath_runtime::control_socket::ControlResponse;

    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Interactions {
        interactions: Vec::new(),
    });
    let oversized =
        run_query_with_daemon(control, "{ openInteractions(first: 100000) { total } }").await;
    assert!(
        oversized
            .errors
            .first()
            .is_some_and(|error| error.message.contains("page cap")),
        "{:?}",
        oversized.errors
    );

    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Interactions {
        interactions: Vec::new(),
    });
    let deep = run_query_with_daemon(
        control,
        &format!(
            "{{ openInteractions(filter: {}) {{ total }} }}",
            too_deep("{ stageName: { eq: \"build\" } }")
        ),
    )
    .await;
    assert!(
        deep.errors
            .first()
            .is_some_and(|error| error.message.contains("levels deep")),
        "{:?}",
        deep.errors
    );
}

/// A filter too deep to walk is refused wherever a run filter is read, by the
/// one render that every surface takes its digest from.
#[tokio::test]
async fn a_run_filter_too_deep_to_walk_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-deep-filter", |_d| async move {
        create_run(&meta_at("run-a", 100)).expect("the run is written");
        let deep = too_deep("{ blueprintName: { eq: \"test-agent\" } }");

        // The listing itself, which compiles the filter into a predicate.
        let listing = run_query(&format!("{{ runs(filter: {deep}) {{ total }} }}")).await;
        assert!(
            listing
                .errors
                .first()
                .is_some_and(|error| error.message.contains("levels deep")),
            "{:?}",
            listing.errors
        );

        // And a run's own interactions, which digest their filter the same way.
        let nested = run_query(&format!(
            "{{ run(id: \"run-a\") {{ interactions(filter: {}) {{ total }} }} }}",
            too_deep("{ stageName: { eq: \"build\" } }")
        ))
        .await;
        assert!(
            nested
                .errors
                .first()
                .is_some_and(|error| error.message.contains("levels deep")),
            "{:?}",
            nested.errors
        );
    })
    .await;
}

/// A script whose file is not there says so when its source is asked for, and
/// a reference naming a blueprint no name could belong to is refused before
/// any directory is walked.
///
/// A mime check is the one script named by a registry row rather than found on
/// disk, so it is the one that can be listed and still have nothing to read.
#[tokio::test]
async fn a_script_with_no_file_and_a_reference_with_a_bad_blueprint_are_refused() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let config_dir = crate::config::mime_types_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .expect("a config directory");
        std::fs::create_dir_all(&config_dir).expect("the config directory");
        std::fs::write(
            config_dir.join("mime_types.toml"),
            "[\"model/obj\"]\ncheck = \"checks/gone.rhai\"\n",
        )
        .expect("a row naming a check");

        let listed = run_query("{ scripts { results { kind name compiles } } }").await;
        assert!(listed.errors.is_empty(), "{:?}", listed.errors);
        let json = serde_json::to_value(&listed.data).expect("data serializes");
        let found = json["scripts"]["results"]
            .as_array()
            .expect("results")
            .iter()
            .find(|script| script["kind"] == "MIME_CHECK")
            .expect("the row's check is listed");
        assert_eq!(found["compiles"], false, "there is nothing to compile");

        // Asking for its source is the read that has nothing to read.
        let source = run_query("{ scripts { results { kind content } } }").await;
        assert!(
            source
                .errors
                .first()
                .is_some_and(|error| error.message.contains("No such script")),
            "{:?}",
            source.errors
        );

        // And a blueprint name that is not a name at all is refused where the
        // reference is resolved.
        let reference = run_query(
            r#"{ script(ref: { kind: TOOL, name: "summarise", blueprintName: "../escape" })
                 { id } }"#,
        )
        .await;
        assert!(
            reference
                .errors
                .first()
                .is_some_and(|error| error.message.contains("Invalid agent name")),
            "{:?}",
            reference.errors
        );
    })
    .await;
}
