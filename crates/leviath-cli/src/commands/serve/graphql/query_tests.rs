//! Tests for the read side of the schema.
//!
//! Whole queries run against the schema over an isolated runs directory, so
//! what is asserted is the answer a client gets rather than the shape of an
//! intermediate. The filters each have their own tests beside the input they
//! belong to.

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema, Variables};

use super::{Query, waiver_word};
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

/// The ids a `runs` answer carries, in the order they came back.
fn ids_of(data: &async_graphql::Value, field: &str) -> Vec<String> {
    let json = serde_json::to_value(data).expect("data serializes");
    json[field]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| edge["node"]["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A blueprint reference the schema cannot read is refused before the query
/// runs at all.
///
/// Every field that takes one takes the same reference, so they are checked
/// together: a reference says which installed blueprint, and manifest text is
/// not part of saying that.
#[tokio::test]
async fn a_blueprint_reference_the_schema_cannot_read_is_refused() {
    for query in [
        r#"{ tools(blueprint: { content: "[agent]" }) { tools { name } } }"#,
        r#"{ scripts(blueprint: { content: "[agent]" }) { name } }"#,
        r#"{ tools(blueprint: {}) { tools { name } } }"#,
        r#"{ scripts(blueprint: {}) { name } }"#,
        r#"{ runs(filter: { blueprint: {} }) { edges { node { id } } } }"#,
    ] {
        let answer = run_query(query).await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("blueprint"),
            "{query}: {}",
            error.message
        );
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
            for query in [
                format!(
                    r#"{{ tools(blueprint: {{ name: "drifted", digest: "{stale}" }})
                         {{ tools {{ name }} }} }}"#
                ),
                format!(
                    r#"{{ scripts(blueprint: {{ name: "drifted", digest: "{stale}" }})
                         {{ name }} }}"#
                ),
            ] {
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
        r#"query { validateBlueprint(name: "coder") { valid } }"#,
        r#"query { validateBlueprint(content: "[agent]", digest: "abc") { valid } }"#,
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
                    edges { cursor node { id blueprintName status task } }
                    pageInfo { hasNextPage endCursor }
                    total
                    scanTruncated
                    missing
                    serverTime
                } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        assert_eq!(ids_of(&answer.data, "runs"), vec!["run-4", "run-3"]);

        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 5);
        assert_eq!(json["runs"]["scanTruncated"], false);
        assert_eq!(json["runs"]["pageInfo"]["hasNextPage"], true);
        assert!(json["runs"]["serverTime"].as_i64().unwrap_or_default() > 0);
        assert_eq!(json["runs"]["edges"][0]["node"]["status"], "STARTING");
        assert_eq!(
            json["runs"]["edges"][0]["node"]["blueprintName"],
            "test-agent"
        );
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

        let first = run_query("{ runs(first: 2) { pageInfo { endCursor } } }").await;
        let json = serde_json::to_value(&first.data).expect("data serializes");
        let cursor = json["runs"]["pageInfo"]["endCursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();
        let next = schema
            .execute(
                Request::new("query($after: Cursor) { runs(first: 2, after: $after) { edges { node { id } } } }")
                    .variables(Variables::from_json(serde_json::json!({ "after": cursor }))),
            )
            .await;
        assert!(next.errors.is_empty(), "{:?}", next.errors);
        assert_eq!(ids_of(&next.data, "runs"), vec!["run-1", "run-0"]);
    })
    .await;
}

/// An id that names nothing is reported rather than thrown: one dead id in a
/// batch must not cost a client the rest of the batch.
#[tokio::test]
async fn an_unknown_id_lands_in_missing() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-missing", |_d| async move {
        create_run(&meta_at("run-real", 100)).expect("run written");

        let answer = run_query(
            r#"{ runs(filter: { ids: ["run-real", "run-ghost"] }) {
                    edges { node { id } }
                    missing
                    total
                } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        assert_eq!(ids_of(&answer.data, "runs"), vec!["run-real"]);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["missing"][0], "run-ghost");
        assert_eq!(json["runs"]["total"], 1);
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
            r#"{ runs(first: 2, filter: { status: ERROR }) {
                    edges { node { id } } pageInfo { hasNextPage endCursor } total
                } }"#,
        )
        .await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(ids_of(&first.data, "runs"), vec!["run-4", "run-2"]);
        let json = serde_json::to_value(&first.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 3, "the count describes the filter");
        let cursor = json["runs"]["pageInfo"]["endCursor"]
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
                       filter: { status: ERROR }) { edges { node { id } } } }",
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
                       filter: { status: COMPLETE }) { total } }",
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
                   { status: ERROR },
                   { title: { endsWith: "number 1" } }
                 ] }) { edges { node { id } } total } }"#,
        )
        .await;
        assert!(either.errors.is_empty(), "{:?}", either.errors);
        let json = serde_json::to_value(&either.data).expect("data serializes");
        assert_eq!(json["runs"]["total"], 4);

        // And `not` around it selects exactly the rest.
        let rest = run_query(
            r#"{ runs(filter: { not: { or: [
                   { status: ERROR },
                   { title: { endsWith: "number 1" } }
                 ] } }) { edges { node { id } } } }"#,
        )
        .await;
        assert_eq!(ids_of(&rest.data, "runs"), vec!["run-3"]);
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
            r#"{ runs(filter: { ids: ["run-failed", "run-fine", "run-ghost"], status: ERROR }) {
                    edges { node { id } } missing total
                } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        assert_eq!(ids_of(&answer.data, "runs"), vec!["run-failed"]);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["missing"][0], "run-ghost");
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
            r#"{ runs(filter: { ids: ["root", "worker", "grandchild"],
                                descendantOf: "root" }) {
                    edges { node { id } } total
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

/// A refused request names what was wrong and carries the code a client
/// branches on, rather than an uncoded message it would have to read.
#[tokio::test]
async fn a_refused_request_carries_its_code() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runs-refused", |_d| async move {
        let answer = run_query("{ runs(first: 100000) { total } }").await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("page-size cap"), "{}", error.message);
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
            r#"{ runs(filter: { parent: "root" }) { edges { node { id parentId } } total } }"#,
        )
        .await;
        assert!(children.errors.is_empty(), "{:?}", children.errors);
        assert_eq!(
            ids_of(&children.data, "runs"),
            vec!["worker-2", "worker-1", "worker-0"]
        );

        let roots =
            run_query("{ runs(filter: { scope: TOP_LEVEL }) { edges { node { id } } } }").await;
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
        meta.blueprint_digest = Some(
            crate::commands::serve::core::blueprints::digest_of(ran),
        );
        create_run(&meta).expect("run written");
        std::fs::write(
            crate::commands::serve::core::blueprints::run_dir(&meta.run_id)
                .join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
            ran,
        )
        .expect("snapshot written");

        let answer = run_query(
            "{ runs { edges { node { blueprintDigest blueprint { name version source digest } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["edges"][0]["node"];
        assert_eq!(node["blueprint"]["version"], "1.0.0", "what ran, not what is installed");
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

        let answer = run_query(
            "{ runs { edges { node { blueprintDigest blueprint { version source } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["edges"][0]["node"];
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

        let answer = run_query("{ runs { edges { node { id blueprint { name } } } } }").await;
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let edges = json["runs"]["edges"].as_array().expect("edges");
        assert_eq!(edges.len(), 2, "both runs are still on the page");
        assert!(edges[0]["node"]["blueprint"].is_null(), "the field is null");
        assert_eq!(edges[0]["node"]["id"], "coder-1788924523-gone00");
        // The edges resolve side by side, so which unreadable blueprint is
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

/// The installed blueprints, by name, with the digest that says which bytes
/// they are.
#[tokio::test]
async fn the_blueprint_listing_reads_what_is_installed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        for (name, version) in [("alpha", "1.0.0"), ("beta", "2.0.0")] {
            let dir = agents.path().join(name);
            std::fs::create_dir_all(&dir).expect("agent dir");
            std::fs::write(
                dir.join(leviath_core::files::MANIFEST_FILENAME),
                format!("[agent]\nname = \"{name}\"\nversion = \"{version}\"\n"),
            )
            .expect("manifest written");
        }
        let state = crate::commands::serve::testutil::state_with_agent_paths(vec![
            agents.path().to_path_buf(),
        ]);
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();

        let answer = schema
            .execute(Request::new(
                "{ blueprints { edges { cursor node { name version source digest } } total missing
                            pageInfo { hasNextPage endCursor } } }",
            ))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let listing = &json["blueprints"];
        assert_eq!(listing["total"], 2);
        assert_eq!(listing["edges"][0]["node"]["name"], "alpha");
        assert_eq!(listing["edges"][0]["node"]["version"], "1.0.0");
        // A listing is always the live definition, never a run's frozen copy.
        assert_eq!(listing["edges"][0]["node"]["source"], "INSTALLED");
        assert_eq!(listing["edges"][1]["node"]["name"], "beta");
        assert_eq!(listing["pageInfo"]["hasNextPage"], false);
        assert!(listing["missing"].as_array().map(Vec::is_empty) == Some(true));
    })
    .await;
}

/// A name that is not installed is reported rather than thrown, and a prefix
/// narrows the listing.
#[tokio::test]
async fn an_unknown_blueprint_name_lands_in_missing() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        let dir = agents.path().join("alpha");
        std::fs::create_dir_all(&dir).expect("agent dir");
        std::fs::write(
            dir.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"alpha\"\n",
        )
        .expect("manifest written");
        let state = crate::commands::serve::testutil::state_with_agent_paths(vec![
            agents.path().to_path_buf(),
        ]);
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();

        let answer = schema
            .execute(Request::new(
                r#"{ blueprints(filter: { names: ["alpha", "ghost"] }) { edges { node { name } } missing } }"#,
            ))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["blueprints"]["edges"][0]["node"]["name"], "alpha");
        assert_eq!(json["blueprints"]["missing"][0], "ghost");

        let narrowed = schema
            .execute(Request::new(
                r#"{ blueprints(filter: { query: "ghos" }) { total edges { node { name } } } }"#,
            ))
            .await;
        let json = serde_json::to_value(&narrowed.data).expect("data serializes");
        assert_eq!(json["blueprints"]["total"], 0, "the prefix matches nothing");
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
        let agents = tempfile::tempdir().expect("a temp dir");
        for name in ["alpha", "beta", "gamma"] {
            let dir = agents.path().join(name);
            std::fs::create_dir_all(&dir).expect("agent dir");
            std::fs::write(
                dir.join(leviath_core::files::MANIFEST_FILENAME),
                format!("[agent]\nname = \"{name}\"\n"),
            )
            .expect("manifest written");
        }
        let state = crate::commands::serve::testutil::state_with_agent_paths(vec![
            agents.path().to_path_buf(),
        ]);
        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state)
            .finish();

        let first = schema
            .execute(Request::new(
                "{ blueprints(first: 2) { edges { node { name } } pageInfo { hasNextPage endCursor } } }",
            ))
            .await;
        let json = serde_json::to_value(&first.data).expect("data serializes");
        assert_eq!(json["blueprints"]["edges"].as_array().map(Vec::len), Some(2));
        assert_eq!(json["blueprints"]["edges"][0]["node"]["name"], "alpha");
        assert_eq!(json["blueprints"]["pageInfo"]["hasNextPage"], true);
        let cursor = json["blueprints"]["pageInfo"]["endCursor"]
            .as_str()
            .expect("a cursor")
            .to_string();

        let rest = schema
            .execute(
                Request::new(
                    "query($after: Cursor) { blueprints(first: 2, after: $after) {
                       edges { node { name } } pageInfo { hasNextPage endCursor } } }",
                )
                .variables(Variables::from_json(
                    serde_json::json!({ "after": cursor.clone() }),
                )),
            )
            .await;
        assert!(rest.errors.is_empty(), "{:?}", rest.errors);
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        assert_eq!(json["blueprints"]["edges"][0]["node"]["name"], "gamma");
        assert_eq!(json["blueprints"]["pageInfo"]["hasNextPage"], false);
        assert!(
            json["blueprints"]["pageInfo"]["endCursor"].is_null(),
            "no cursor is minted for a page nothing follows"
        );

        // A cursor minted for one filter cannot resume another: the walk it
        // names is not the walk being asked for.
        let crossed = schema
            .execute(
                Request::new(
                    r#"query($after: Cursor) { blueprints(first: 2, after: $after,
                         filter: { query: "a" }) { total } }"#,
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

/// The catalogue fields answer from what this machine has configured.
///
/// A daemon-less state configures no provider, so the honest answer is empty
/// lists rather than an error: "nothing configured" is a state, not a failure.
#[tokio::test]
async fn the_catalogue_answers_for_an_unconfigured_machine() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer =
            run_query("{ models { id provider } providers { id display enabled signedIn } }").await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["models"].as_array().map(Vec::len), Some(0));
        // Every provider Leviath can sign in to is listed, configured or not,
        // which is what a settings screen needs to offer them.
        let providers = json["providers"].as_array().expect("providers");
        assert!(!providers.is_empty(), "the sign-in providers are listed");
        assert!(
            providers.iter().all(|p| p["enabled"] == false),
            "nothing is configured here: {providers:?}"
        );
    })
    .await;
}

/// The tool inventory carries the built-ins, the group tokens a blueprint may
/// name, and whatever could not be offered.
#[tokio::test]
async fn the_tool_inventory_lists_tools_and_groups() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            "{ tools { tools { name origin } groups { name description } skipped { path reason } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let tools = json["tools"]["tools"].as_array().expect("tools");
        assert!(
            tools.iter().any(|t| t["name"] == "read_file"),
            "the built-ins are there"
        );
        let groups = json["tools"]["groups"].as_array().expect("groups");
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
            run_query(r#"{ tools(blueprint: { name: "coder" }) { skipped { path reason } } }"#)
                .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let skipped = json["tools"]["skipped"].as_array().expect("skipped");
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
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer =
            run_query(r#"{ tools(blueprint: { name: "../etc" }) { tools { name } } }"#).await;
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
            "{ runs { edges { node { finalOutput { content format stage submittedAt truncated } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let output = &json["runs"]["edges"][0]["node"]["finalOutput"];
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
            "{ runs { edges { node { finalOutput { content } context { totalTokens }
                                     stages { name } waitReason { reason }
                                     flags { emptyOutput modifiedFileCount } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["edges"][0]["node"];
        assert!(node["finalOutput"].is_null(), "nothing submitted");
        assert!(node["context"].is_null(), "no window written yet");
        assert!(node["waitReason"].is_null(), "not parked");
        assert_eq!(node["stages"].as_array().map(Vec::len), Some(0));
        assert_eq!(node["flags"]["modifiedFileCount"], 0);
    })
    .await;
}

/// The blueprint listing is bounded by the same page cap the run listing is.
#[tokio::test]
async fn the_blueprint_listing_refuses_an_oversized_page() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query("{ blueprints(first: 100000) { total } }").await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("page-size cap"), "{}", error.message);
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
            "{ runs { edges { node { context { totalTokens maxTokens stageName
                                              regions { name tokens } } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let window = &json["runs"]["edges"][0]["node"]["context"];
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

        let answer = run_query("{ runs { edges { node { blueprint { name } } } } }").await;
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

/// A run's children are paged, and the page says whether a level was cut.
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
            r#"{ runs(filter: { ids: ["root"] }) { edges { node {
                   children(first: 2) { total hasNextPage edges { node { id parentId } } }
                 } } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let children = &json["runs"]["edges"][0]["node"]["children"];
        assert_eq!(children["total"], 3);
        assert_eq!(children["hasNextPage"], true, "the level was cut");
        assert_eq!(children["edges"].as_array().map(Vec::len), Some(2));
        assert_eq!(children["edges"][0]["node"]["parentId"], "root");

        let rest = run_query(
            r#"{ runs(filter: { ids: ["root"] }) { edges { node {
                   children(first: 2, skip: 2) { hasNextPage edges { node { id } } }
                 } } } }"#,
        )
        .await;
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        let children = &json["runs"]["edges"][0]["node"]["children"];
        assert_eq!(children["edges"].as_array().map(Vec::len), Some(1));
        assert_eq!(children["hasNextPage"], false);

        let refused = run_query(
            r#"{ runs(filter: { ids: ["root"] }) { edges { node { children(skip: -1) { total } } } } }"#,
        )
        .await;
        assert!(
            refused
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("negative"),
            "{:?}",
            refused.errors
        );
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
            r#"{ runs(filter: { ids: ["root"] }) { edges { node {
                   treeStatus { depth descendantCount rollup { promptTokens } }
                 } } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let tree = &json["runs"]["edges"][0]["node"]["treeStatus"];
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
            r#"{ runs { edges { node { treeStatus { depth descendantCount
                                                    rollup { promptTokens } } } } } }"#,
        )
        .await;
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let tree = &json["runs"]["edges"][0]["node"]["treeStatus"];
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

        let output = run_query("{ runs { edges { node { logs(stageIndex: 0) } } } }").await;
        assert!(output.errors.is_empty(), "{:?}", output.errors);
        let json = serde_json::to_value(&output.data).expect("data serializes");
        assert!(
            json["runs"]["edges"][0]["node"]["logs"]
                .as_str()
                .unwrap_or_default()
                .contains("first stage output"),
            "{json}"
        );

        let operational =
            run_query("{ runs { edges { node { logs(stageIndex: 0, operational: true) } } } }")
                .await;
        let json = serde_json::to_value(&operational.data).expect("data serializes");
        assert!(
            json["runs"]["edges"][0]["node"]["logs"]
                .as_str()
                .unwrap_or_default()
                .contains("[tool] read_file"),
            "{json}"
        );

        let every =
            run_query("{ runs { edges { node { logs(allStages: true, tailBytes: 100) } } } }")
                .await;
        assert!(every.errors.is_empty(), "{:?}", every.errors);
    })
    .await;
}

/// Asking for one stage and every stage at once is a contradiction, and so is a
/// negative index or window.
#[tokio::test]
async fn the_log_selectors_refuse_a_contradiction() {
    crate::runstate::with_isolated_runs_dir_async("graphql-logs-refused", |_d| async move {
        create_run(&meta_at("coder-1788924523-bad999", 100)).expect("run written");

        let both =
            run_query("{ runs { edges { node { logs(stageIndex: 0, allStages: true) } } } }").await;
        assert!(
            both.errors
                .first()
                .expect("a refusal")
                .message
                .contains("cannot be combined"),
            "{:?}",
            both.errors
        );

        let negative = run_query("{ runs { edges { node { logs(stageIndex: -1) } } } }").await;
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

        let window = run_query("{ runs { edges { node { logs(tailBytes: -1) } } } }").await;
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
        let stored = store.put(&meta.run_id, &picture, &registry).expect("stored");
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
            "{ runs { edges { node { blobs { sha256 mimeType name size stored regions url } } } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let blob = &json["runs"]["edges"][0]["node"]["blobs"][0];
        assert_eq!(blob["mimeType"], "image/png");
        assert_eq!(blob["name"], "shot.png");
        assert_eq!(blob["stored"], true);
        assert_eq!(blob["regions"][0], "files");
        let url = blob["url"].as_str().expect("a link");
        assert!(url.contains("/blobs/"), "{url}");
        assert!(url.contains("sig="), "it carries its own grant: {url}");
    })
    .await;
}

/// A file link names the file and the grant, and says when it is a download.
#[tokio::test]
async fn a_file_link_carries_its_path_and_its_grant() {
    crate::runstate::with_isolated_runs_dir_async("graphql-file-url", |_d| async move {
        create_run(&meta_at("coder-1788924523-file00", 100)).expect("run written");

        let answer = run_query(
            r#"{ runs { edges { node {
                   inline: fileUrl(path: "out.png")
                   saved: fileUrl(path: "out.png", download: true)
                 } } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let node = &json["runs"]["edges"][0]["node"];
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
            "{ config { defaultProvider providerOrder configuredProviders apiVersion
                        capabilities agentPaths mcpServerCount
                        limits { maxPageSize maxIds maxUploadBytes requestTimeoutSecs }
                        configError { message } configMtime } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let config = &json["config"];
        assert!(
            config["capabilities"]
                .as_array()
                .expect("capabilities")
                .iter()
                .any(|c| c == "graphql"),
            "this server announces the surface a client is reading it over"
        );
        assert_eq!(config["limits"]["maxPageSize"], 200);
        assert!(config["limits"]["maxIds"].as_i64().unwrap_or_default() > 0);
        assert!(
            config["apiVersion"].as_str().unwrap_or_default().len() > 2,
            "{config}"
        );
        // Nothing configured in an isolated home, which is a state rather than
        // a failure, and no key material either way.
        assert_eq!(
            config["configuredProviders"].as_array().map(Vec::len),
            Some(0)
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
        let answer = run_query("{ doctor { ok checks { name ok detail } } }").await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
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

/// The MCP servers, the yolo profiles, the mime rows and the scripts, from a
/// machine with none of them configured.
///
/// Empty is the honest answer here, and each field says where it read from
/// rather than implying the file is broken.
#[tokio::test]
async fn the_machine_listings_answer_for_a_bare_install() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = run_query(
            "{ mcpServers { name transport endpoint auth }
               yoloProfiles { path exists error profiles { name default } }
               mime { mimeType source family isText extensions }
               scripts { kind name foundAt blueprint } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["mcpServers"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            json["yoloProfiles"]["exists"], false,
            "no profiles file yet"
        );
        assert!(json["yoloProfiles"]["error"].is_null(), "and no failure");
        assert!(
            json["yoloProfiles"]["path"]
                .as_str()
                .unwrap_or_default()
                .ends_with("yolo.toml"),
            "it says where it looked"
        );
        // The built-in mime rows are always there: they are compiled in.
        let mime = json["mime"].as_array().expect("mime rows");
        assert!(
            mime.iter().any(|row| row["mimeType"] == "image/png"),
            "the built-in rows are listed"
        );
        assert!(
            mime.iter()
                .all(|row| !row["source"].as_str().unwrap_or_default().is_empty()),
            "each row says where it came from"
        );
        assert!(json["scripts"].is_array());
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
            "query Dirs($path: String!) { directories(path: $path)
               { path parent home cwd entries } }",
            &path,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let listing = &json["directories"];
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
            "query Dirs($path: String!) { directories(path: $path, hidden: true) { entries } }",
            &path,
        )
        .await;
        let json = serde_json::to_value(&with_hidden.data).expect("data serializes");
        assert!(
            json["directories"]["entries"]
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
        let relative = run_query(r#"{ directories(path: "relative/path") { path } }"#).await;
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
            "query Dirs($path: String!) { directories(path: $path) { path } }",
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
            run_query(r#"{ runs(filter: { parent: "root" }) { edges { node { id } } } }"#).await;
        assert_eq!(ids_of(&answer.data, "runs"), vec!["worker".to_string()]);

        // The whole subtree: every level, and not the root itself.
        let answer =
            run_query(r#"{ runs(filter: { descendantOf: "root" }) { edges { node { id } } } }"#)
                .await;
        let mut under = ids_of(&answer.data, "runs");
        under.sort();
        assert_eq!(under, vec!["grandchild".to_string(), "worker".to_string()]);

        // Roots, and its mirror.
        let answer =
            run_query("{ runs(filter: { scope: TOP_LEVEL }) { edges { node { id } } } }").await;
        let mut roots = ids_of(&answer.data, "runs");
        roots.sort();
        assert_eq!(roots, vec!["other".to_string(), "root".to_string()]);
        let answer =
            run_query("{ runs(filter: { scope: SUB_AGENTS }) { edges { node { id } } } }").await;
        let mut subs = ids_of(&answer.data, "runs");
        subs.sort();
        assert_eq!(subs, vec!["grandchild".to_string(), "worker".to_string()]);

        // By blueprint, which composes with the rest.
        let answer = run_query(
            r#"{ runs(filter: { blueprint: { name: "researcher" } }) { edges { node { id } } total } }"#,
        )
        .await;
        assert_eq!(ids_of(&answer.data, "runs"), vec!["other".to_string()]);
        let answer = run_query(
            r#"{ runs(filter: { blueprint: { name: "coder" }, scope: SUB_AGENTS })
                 { edges { node { id } } } }"#,
        )
        .await;
        assert_eq!(ids_of(&answer.data, "runs").len(), 2);
        // A blueprint nothing matches is an empty page, not a refusal: a
        // blueprint with no runs yet is an ordinary answer.
        let answer = run_query(r#"{ runs(filter: { blueprint: { name: "nope" } }) { total } }"#).await;
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

        let both =
            run_query(r#"{ runs(filter: { parent: "root", descendantOf: "root" }) { edges { node { id } } } }"#)
                .await;
        assert!(both.errors.is_empty(), "{:?}", both.errors);
        assert_eq!(ids_of(&both.data, "runs"), vec!["worker".to_string()]);

        let contradiction =
            run_query(r#"{ runs(filter: { parent: "root", scope: TOP_LEVEL }) { total } }"#).await;
        assert!(contradiction.errors.is_empty(), "{:?}", contradiction.errors);
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
            .execute(async_graphql::Request::new("{ config { adminEnabled } }"))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["config"]["adminEnabled"], allow_admin);
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

            let answer = run_query("{ scripts { kind name foundAt blueprint } }").await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let scripts = json["scripts"].as_array().expect("the scripts");
            let tool = scripts
                .iter()
                .find(|script| script["name"] == "summarize")
                .expect("the tool that was just written");
            assert_eq!(tool["kind"], "tool");
            assert_eq!(tool["foundAt"], "global");
            assert!(
                tool["blueprint"].is_null(),
                "a global tool belongs to nobody"
            );

            // An agent nothing knows about is not a refusal: the global scripts
            // are still the answer, and that agent simply has none.
            let scoped = run_query(r#"{ scripts(blueprint: { name: "coder" }) { name } }"#).await;
            assert!(scoped.errors.is_empty(), "{:?}", scoped.errors);
        })
        .await;
    }

    /// The tools listing carries the group tokens a stage can name, beside the
    /// tools themselves.
    #[tokio::test]
    async fn the_tools_listing_carries_the_group_tokens() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let answer = run_query(
                "{ tools { tools { name origin description
                       ... on ScriptTool { path blueprint requires } }
                     groups { name description } skipped { path reason } } }",
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let tools = json["tools"]["tools"].as_array().expect("the tools");
            assert!(!tools.is_empty(), "a build ships built-in tools");
            assert!(
                tools
                    .iter()
                    .all(|tool| tool["name"].as_str().is_some_and(|n| !n.is_empty()))
            );
            let groups = json["tools"]["groups"].as_array().expect("the groups");
            assert!(
                groups.iter().any(|group| group["name"] == "@builtin"),
                "the tokens a stage can name: {groups:?}"
            );
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
                    let answer =
                        run_query("{ mcpServers { name transport endpoint configError auth } }")
                            .await;
                    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
                    let json = serde_json::to_value(&answer.data).expect("data serializes");
                    assert_eq!(json["mcpServers"][0]["name"], "docs");
                    assert_eq!(json["mcpServers"][0]["transport"], "STDIO");
                    assert_eq!(json["mcpServers"][0]["auth"], "NOT_APPLICABLE");
                    assert!(json["mcpServers"][0]["configError"].is_null());
                    assert_eq!(json["mcpServers"][0]["endpoint"], "docs-mcp");
                })
                .await;
        })
        .await;
    }

    /// The profiles come back with what each waives, counted.
    #[tokio::test]
    async fn the_yolo_profiles_report_what_they_waive() {
        crate::commands::serve::testutil::with_home(|_home| async move {
            let path = crate::yolo::yolo_path();
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
            std::fs::write(&path, crate::commands::yolo::EXAMPLE_TOML).expect("the profiles");

            let answer = run_query(
                "{ yoloProfiles { path exists error
                     profiles { name default questions checkpoints gate toolRules shellRules } } }",
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            assert_eq!(json["yoloProfiles"]["exists"], true);
            let profile = &json["yoloProfiles"]["profiles"][0];
            assert!(
                ["ALLOW", "ASK"].contains(&profile["default"].as_str().expect("a value")),
                "the default is one of the two waivers: {profile}"
            );
            // Three counts each: allow, ask, deny, so a settings list can show
            // how much a profile waives without reading the rules.
            assert_eq!(profile["toolRules"].as_array().map(Vec::len), Some(3));
            assert_eq!(profile["shellRules"].as_array().map(Vec::len), Some(3));
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
                    "{ config { defaultProvider configuredProviders agentPaths mcpServerCount
                         apiVersion capabilities adminEnabled
                         gateways { name baseUrl hasApiKey kind }
                         limits { maxPageSize maxIds maxFileBytes maxListingEntries
                             maxSearchScan maxHistoryLimit maxConcurrentRequests
                             maxUploadBytes requestTimeoutSecs } } }",
                ))
                .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let config = &json["config"];
            assert_eq!(config["defaultProvider"], "local");
            let gateway = &config["gateways"][0];
            assert_eq!(gateway["name"], "local");
            // The key is a boolean and never a value: a console needs to know
            // whether one is configured and never needs the key itself.
            assert_eq!(gateway["hasApiKey"], true);
            let rendered = serde_json::to_string(config).expect("it serializes");
            assert!(!rendered.contains("sk-secret"), "no secret travels");
            assert!(
                config["limits"]["maxPageSize"]
                    .as_i64()
                    .is_some_and(|n| n > 0)
            );
            assert!(
                config["capabilities"]
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
        let answer = run_query("{ openInteractions { runId request { prompt } } }").await;
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
                r#"{ runs(filter: { query: "parser", queryIn: [META, LOGS, CONTEXT, JOURNAL, FILES] })
                     { edges { node { id } highlights { field snippet stage } } scanTruncated } }"#,
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            assert_eq!(ids_of(&answer.data, "runs"), vec!["searchable".to_string()]);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let highlights = json["runs"]["edges"][0]["highlights"]
                .as_array()
                .expect("highlights");
            assert!(
                highlights
                    .iter()
                    .any(|hit| hit["snippet"].as_str().is_some_and(|s| s.contains("parser"))),
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
                    "{ config { defaultProvider configMtime
                         configError { kind path message line column key since note } } }",
                ))
                .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let error = &json["config"]["configError"];
            assert_eq!(error["kind"], "parse");
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
            assert!(json["config"]["defaultProvider"].as_str().is_some());
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
                r#"{ tools(blueprint: { name: "coder" }) { tools { name origin
                     ... on ScriptTool { path blueprint requires } } } }"#,
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let tools = json["tools"]["tools"].as_array().expect("the tools");
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
                "{ yoloProfiles { profiles { name default questions checkpoints gate } } }",
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let profile = &json["yoloProfiles"]["profiles"][0];
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
                r#"{ runs(filter: { query: "parser", queryIn: [LOGS] })
                     { edges { highlights { field snippet stage } } } }"#,
            )
            .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let highlights = json["runs"]["edges"][0]["highlights"]
                .as_array()
                .expect("highlights");
            let hit = highlights
                .iter()
                .find(|hit| hit["stage"].as_i64().is_some())
                .expect("a match that knows its stage");
            assert_eq!(hit["stage"], 0, "{hit}");
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

        let answer = run_query("{ mcpServers { name } }").await;
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
    let answer = run_query(r#"{ scripts(blueprint: { name: "../../etc" }) { name } }"#).await;
    let message = &answer.errors.first().expect("a refusal").message;
    assert!(message.contains("Invalid agent name"), "{message}");
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
            "{ runs(first: 2, filter: { ascending: true }) {
                 pageInfo { hasNextPage endCursor }
                 edges { cursor node { id } }
               } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let page = &json["runs"];
        assert_eq!(page["edges"][0]["node"]["id"], "first", "oldest first");
        assert!(
            page["edges"][0]["cursor"]
                .as_str()
                .is_some_and(|c| !c.is_empty()),
            "every edge carries where to resume from"
        );

        // The cursor resumes the same order rather than starting again.
        let cursor = page["pageInfo"]["endCursor"].as_str().expect("a cursor");
        let answer = run_query(&format!(
            r#"{{ runs(first: 2, after: "{cursor}", filter: {{ ascending: true }}) {{
                 edges {{ node {{ id }} }} }} }}"#
        ))
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["runs"]["edges"][0]["node"]["id"], "third");
    })
    .await;
}

/// Both values a profile's default can be.
///
/// Two, and neither is a bare on or off: a profile waives a prompt, it never
/// adds a refusal, so there is no third value for denying anything.
#[test]
fn a_profiles_default_is_one_of_two_waivers() {
    use super::super::types::machine::YoloWaiver;
    assert_eq!(
        waiver_word(crate::yolo::rules::Waiver::Allow),
        YoloWaiver::Allow
    );
    assert_eq!(
        waiver_word(crate::yolo::rules::Waiver::Ask),
        YoloWaiver::Ask
    );
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
        .execute(Request::new("{ config { agentPaths } }"))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    let paths: Vec<&str> = json["config"]["agentPaths"]
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
            r#"query {{ validateBlueprint(content: "{good}") {{ valid errors warnings }} }}"#
        ))
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["validateBlueprint"]["valid"], true);
        assert_eq!(
            json["validateBlueprint"]["errors"].as_array().map(Vec::len),
            Some(0)
        );

        let bad =
            run_query(r#"query { validateBlueprint(content: "not a manifest") { valid errors } }"#)
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
        r#"query { validateConfigKey(provider: "anthropic", key: "sk-ant-abc")
             { valid message } }"#,
    )
    .await;
    assert!(good.errors.is_empty(), "{:?}", good.errors);
    let json = serde_json::to_value(&good.data).expect("data serializes");
    assert_eq!(json["validateConfigKey"]["valid"], true);
    assert!(json["validateConfigKey"]["message"].is_null());

    let wrong = run_query(
        r#"query { validateConfigKey(provider: "anthropic", key: "nope") { valid message } }"#,
    )
    .await;
    let json = serde_json::to_value(&wrong.data).expect("data serializes");
    assert_eq!(json["validateConfigKey"]["valid"], false);
    assert!(
        json["validateConfigKey"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("sk-ant-")),
        "it says what the format is"
    );

    // The address is judged before the key: a key cannot be judged beyond being
    // present until there is somewhere to send it.
    let bad_url = run_query(
        r#"query { validateConfigKey(provider: "anthropic", key: "sk-ant-abc",
             baseUrl: "not a url") { valid message } }"#,
    )
    .await;
    let json = serde_json::to_value(&bad_url.data).expect("data serializes");
    assert_eq!(json["validateConfigKey"]["valid"], false);

    let compiles = run_query(
        r#"query { validateScript(kind: "tool",
             content: "// @tool summarize\n// @description sums up\n\"ok\"")
             { valid error } }"#,
    )
    .await;
    assert!(compiles.errors.is_empty(), "{:?}", compiles.errors);
    let json = serde_json::to_value(&compiles.data).expect("data serializes");
    assert_eq!(json["validateScript"]["valid"], true);

    let broken =
        run_query(r#"query { validateScript(kind: "tool", content: "fn (") { valid error } }"#)
            .await;
    let json = serde_json::to_value(&broken.data).expect("data serializes");
    assert_eq!(json["validateScript"]["valid"], false);
    assert!(json["validateScript"]["error"].as_str().is_some());

    // An unknown registry is a refusal rather than a verdict: there is no
    // compiler to have an opinion.
    let unknown =
        run_query(r#"query { validateScript(kind: "model_provider", content: "") { valid } }"#)
            .await;
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

        let listed = run_query("{ yoloProfiles { profiles { name } } }").await;
        let json = serde_json::to_value(&listed.data).expect("data serializes");
        let name = json["yoloProfiles"]["profiles"][0]["name"]
            .as_str()
            .expect("a profile")
            .to_string();

        let decided = run_query(&format!(
            r#"query {{ testYoloProfile(call: {{ profile: "{name}", tool: "read_file" }})
                     {{ profile tool configured policy reason }} }}"#
        ))
        .await;
        assert!(decided.errors.is_empty(), "{:?}", decided.errors);
        let json = serde_json::to_value(&decided.data).expect("data serializes");
        assert_eq!(json["testYoloProfile"]["profile"], name);
        assert_eq!(json["testYoloProfile"]["tool"], "read_file");
        assert!(
            ["allow", "ask", "deny"].contains(
                &json["testYoloProfile"]["policy"]
                    .as_str()
                    .expect("a policy")
            ),
            "one of the three words: {json}"
        );

        // A profile that is not in the file is a miss.
        let missing = run_query(
            r#"query { testYoloProfile(call: { profile: "nope", tool: "read_file" })
                 { policy } }"#,
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

/// A manifest that will not parse is a report rather than a written blueprint.
#[tokio::test]
async fn a_blueprint_that_will_not_parse_is_reported_not_written() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let report = run_query(
            r#"query { validateBlueprint(content: "[agent]\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\nentry_stage = \"nope\"\n\n[stages.only]\nmode = \"autonomous\"\n")
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
            r#"query { validateBlueprint(content: "[agent]\nname = \"x\"\n",
                 name: "../elsewhere") { valid } }"#,
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

const JOURNAL_QUERY: &str = "{ journal { healthy appendsAttempted appendsFailed \
     snapshotsFailed queueDepth lastError { runId path message at } } }";

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
    assert_eq!(json["journal"]["healthy"], false);
    assert_eq!(json["journal"]["appendsAttempted"], 12);
    assert_eq!(json["journal"]["appendsFailed"], 3);
    assert_eq!(json["journal"]["snapshotsFailed"], 1);
    assert_eq!(json["journal"]["queueDepth"], 4);
    assert_eq!(json["journal"]["lastError"]["runId"], "run-a");
    assert_eq!(json["journal"]["lastError"]["path"], "/runs/run-a/run.lvr");
    assert_eq!(json["journal"]["lastError"]["message"], "Permission denied");
    assert_eq!(json["journal"]["lastError"]["at"], 1_700_000_000i64);
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
    assert_eq!(json["journal"]["healthy"], true);
    assert_eq!(json["journal"]["lastError"], serde_json::Value::Null);
}

/// Null when the daemon cannot be reached, and null when it answers something
/// else: this reading exists only on the daemon, so there is nothing to fall
/// back to.
#[tokio::test]
async fn an_unreachable_daemon_has_no_journal_to_report() {
    let answer = run_query(JOURNAL_QUERY).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["journal"], serde_json::Value::Null);

    let (control, _dir, _srv) = crate::commands::serve::testutil::fake_daemon(|_| {
        leviath_runtime::control_socket::ControlResponse::Ok { ok: true }
    });
    let answer = query_daemon(control, JOURNAL_QUERY).await;
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["journal"], serde_json::Value::Null);
}
