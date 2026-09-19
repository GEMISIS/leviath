//! Tests for the read side of the schema.
//!
//! The filter and page-size rules are checked directly, because they are the
//! request rejections a client has to be able to rely on. The listing itself
//! runs whole queries against the schema over an isolated runs directory, so
//! what is asserted is the answer a client gets rather than the shape of an
//! intermediate.

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema, Variables};

use super::{Query, RunFilter, RunSort, page_size};
use crate::commands::serve::core::runs::{MAX_IDS, MAX_LIMIT, ParentFilter, SortKey, Source};
use crate::commands::serve::graphql::scalars::Timestamp;
use crate::commands::serve::graphql::types::run::RunStatus;
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

/// Run one query against a schema wired to a daemon-less state.
async fn run_query(query: &str) -> async_graphql::Response {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
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

// ─── page size ──────────────────────────────────────────────────────────────

/// A page size over the cap is refused rather than quietly cut down: a client
/// that asked for 500 and silently got 200 finds out by missing rows.
#[test]
fn a_page_size_is_bounded_at_both_ends() {
    assert_eq!(page_size(50).expect("in range"), 50);
    assert_eq!(page_size(MAX_LIMIT as i32).expect("at the cap"), MAX_LIMIT);
    let over = page_size(MAX_LIMIT as i32 + 1).expect_err("over the cap");
    assert!(over.to_string().contains("page-size cap"), "{over}");
    assert!(page_size(0).is_err(), "zero is not a page");
    assert!(page_size(-1).is_err(), "a negative page size is not a page");
}

// ─── filters ────────────────────────────────────────────────────────────────

/// The defaults: newest first, fifty at a time, searching only what is already
/// in memory.
#[test]
fn the_default_filter_reads_nothing_from_disk() {
    let selection = RunFilter::default()
        .selection(50, None)
        .expect("defaults resolve");
    assert_eq!(selection.limit, 50);
    assert_eq!(selection.sort, SortKey::Started);
    assert!(selection.descending);
    assert_eq!(selection.sources, vec![Source::Meta, Source::Files]);
    assert_eq!(selection.parent, ParentFilter::Any);
    assert!(selection.fields.is_none(), "projection is REST's, not ours");
}

/// Both status spellings feed one filter list, in the daemon's own words.
#[test]
fn statuses_reach_the_core_in_the_daemons_spelling() {
    let selection = RunFilter {
        status: Some(RunStatus::Running),
        status_in: Some(vec![
            RunStatus::WaitingInput,
            RunStatus::CompleteInteractive,
        ]),
        ..Default::default()
    }
    .selection(50, None)
    .expect("statuses resolve");
    assert_eq!(
        selection.statuses,
        vec!["running", "waiting_input", "complete_interactive"]
    );
}

/// The search scopes decide both what is read and what the cursor's digest is
/// taken over, so the request's own spelling is what travels.
#[test]
fn a_search_scope_selects_its_source_and_digests_as_written() {
    let selection = RunFilter {
        query: Some("boom".to_string()),
        query_in: Some(vec![super::SearchScope::Logs, super::SearchScope::Meta]),
        ..Default::default()
    }
    .selection(50, None)
    .expect("scopes resolve");
    assert_eq!(selection.sources, vec![Source::Logs, Source::Meta]);
    assert_eq!(selection.sources_raw, "logs,meta");
    assert!(
        selection
            .sources
            .iter()
            .any(|source| source.reads_filesystem())
    );
}

/// Parentage is one question with two spellings, and asking both at once is a
/// client bug worth saying out loud.
#[test]
fn parentage_is_either_one_run_or_the_top_level() {
    let of = RunFilter {
        parent: Some("root-1".to_string()),
        ..Default::default()
    }
    .selection(50, None)
    .expect("a parent resolves");
    assert_eq!(of.parent, ParentFilter::Of("root-1".to_string()));

    let roots = RunFilter {
        top_level_only: Some(true),
        ..Default::default()
    }
    .selection(50, None)
    .expect("top level resolves");
    assert_eq!(roots.parent, ParentFilter::Roots);

    let both = RunFilter {
        parent: Some("root-1".to_string()),
        top_level_only: Some(true),
        ..Default::default()
    }
    .selection(50, None)
    .expect_err("both at once is refused");
    assert!(both.to_string().contains("topLevelOnly"), "{both}");
}

/// `ids` names exactly what it wants, so combining it with a filter is a
/// request that cannot be honoured both ways.
#[test]
fn a_batch_fetch_refuses_to_be_filtered() {
    let ok = RunFilter::default()
        .selection(50, Some(vec!["run-a".to_string()]))
        .expect("ids alone resolve");
    assert_eq!(ok.ids, Some(vec!["run-a".to_string()]));

    let filtered = RunFilter {
        status: Some(RunStatus::Running),
        ..Default::default()
    }
    .selection(50, Some(vec!["run-a".to_string()]))
    .expect_err("ids with a filter is refused");
    assert!(filtered.to_string().contains("`ids`"), "{filtered}");

    let too_many: Vec<String> = (0..=MAX_IDS).map(|i| format!("run-{i}")).collect();
    let over = RunFilter::default()
        .selection(50, Some(too_many))
        .expect_err("too many ids is refused");
    assert!(over.to_string().contains("at most"), "{over}");
}

/// Sort and order are the two halves of one walk, and both reach the core.
#[test]
fn sort_and_direction_travel_to_the_core() {
    let selection = RunFilter {
        sort: Some(RunSort::Updated),
        ascending: Some(true),
        since: Some(Timestamp(1_700_000_000)),
        ..Default::default()
    }
    .selection(10, None)
    .expect("sort resolves");
    assert_eq!(selection.sort, SortKey::Updated);
    assert!(!selection.descending);
    assert_eq!(selection.since, Some(1_700_000_000));

    let last = RunFilter {
        sort: Some(RunSort::LastProgress),
        ..Default::default()
    }
    .selection(10, None)
    .expect("sort resolves");
    assert_eq!(last.sort, SortKey::LastProgress);
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
                    edges { cursor node { id agentName status task } }
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
        assert_eq!(json["runs"]["edges"][0]["node"]["agentName"], "test-agent");
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
            r#"{ runs(ids: ["run-real", "run-ghost"]) {
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
            run_query("{ runs(filter: { topLevelOnly: true }) { edges { node { id } } } }").await;
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
        let error = answer.errors.first().expect("an error names the field");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string())
        );
        assert!(error.message.contains("agent.leviath"), "{}", error.message);
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
                r#"{ blueprints(exact: ["alpha", "ghost"]) { edges { node { name } } missing } }"#,
            ))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["blueprints"]["edges"][0]["node"]["name"], "alpha");
        assert_eq!(json["blueprints"]["missing"][0], "ghost");

        let narrowed = schema
            .execute(Request::new(
                r#"{ blueprints(query: "ghos") { total edges { node { name } } } }"#,
            ))
            .await;
        let json = serde_json::to_value(&narrowed.data).expect("data serializes");
        assert_eq!(json["blueprints"]["total"], 0, "the prefix matches nothing");
    })
    .await;
}

/// Paging the listing: a page, then the rest, with `hasNextPage` marking the
/// cut.
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
    assert_eq!(json["blueprints"]["pageInfo"]["hasNextPage"], true);
    assert_eq!(json["blueprints"]["pageInfo"]["endCursor"], "beta");

    let rest = schema
        .execute(Request::new(
            "{ blueprints(first: 2, skip: 2) { edges { node { name } } pageInfo { hasNextPage } } }",
        ))
        .await;
    let json = serde_json::to_value(&rest.data).expect("data serializes");
    assert_eq!(json["blueprints"]["edges"][0]["node"]["name"], "gamma");
    assert_eq!(json["blueprints"]["pageInfo"]["hasNextPage"], false);

    let negative = schema
        .execute(Request::new("{ blueprints(skip: -1) { total } }"))
        .await;
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
  })
  .await;
}
