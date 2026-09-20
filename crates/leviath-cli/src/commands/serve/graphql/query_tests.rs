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

/// Every search scope maps to the source it reads, and digests as the request
/// spelled it.
///
/// The table is checked whole because the mapping is the contract: a scope that
/// quietly read a different source would answer a search nobody asked for.
#[test]
fn every_search_scope_maps_to_one_source() {
    use super::SearchScope;
    let cases = [
        (SearchScope::Meta, Source::Meta, "meta"),
        (SearchScope::Files, Source::Files, "files"),
        (SearchScope::Context, Source::Context, "context"),
        (SearchScope::Logs, Source::Logs, "logs"),
        (SearchScope::Journal, Source::Journal, "journal"),
    ];
    for (scope, source, word) in cases {
        let selection = RunFilter {
            query: Some("x".to_string()),
            query_in: Some(vec![scope]),
            ..Default::default()
        }
        .selection(50, None)
        .expect("the scope resolves");
        assert_eq!(selection.sources, vec![source], "{word}");
        assert_eq!(selection.sources_raw, word);
    }
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
            "{ tools { tools { name source } groups { name description } skipped { path reason } } }",
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

        let answer = run_query(r#"{ tools(agent: "coder") { skipped { path reason } } }"#).await;
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
        let answer = run_query(r#"{ tools(agent: "../etc") { tools { name } } }"#).await;
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
            r#"{ runs(ids: ["root"]) { edges { node {
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
            r#"{ runs(ids: ["root"]) { edges { node {
                   children(first: 2, skip: 2) { hasNextPage edges { node { id } } }
                 } } } }"#,
        )
        .await;
        let json = serde_json::to_value(&rest.data).expect("data serializes");
        let children = &json["runs"]["edges"][0]["node"]["children"];
        assert_eq!(children["edges"].as_array().map(Vec::len), Some(1));
        assert_eq!(children["hasNextPage"], false);

        let refused = run_query(
            r#"{ runs(ids: ["root"]) { edges { node { children(skip: -1) { total } } } } }"#,
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
            r#"{ runs(ids: ["root"]) { edges { node {
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
            run_query("{ runs { edges { node { logs(allStages: true, tail: 100) } } } }").await;
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

        let window = run_query("{ runs { edges { node { logs(tail: -1) } } } }").await;
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
               mime { mimeType source family text extensions }
               scripts { kind name source agent } }",
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

        let answer = run_query(&format!(
            r#"{{ directories(path: "{path}") {{ path parent home cwd entries }} }}"#
        ))
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

        let with_hidden = run_query(&format!(
            r#"{{ directories(path: "{path}", hidden: true) {{ entries }} }}"#
        ))
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
    crate::commands::serve::testutil::with_home(|_home| async move {
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

        let missing = run_query(r#"{ directories(path: "/nowhere/at/all") { path } }"#).await;
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
