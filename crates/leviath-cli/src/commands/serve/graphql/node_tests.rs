//! Tests for `Node` and the `node(id:)` lookup.
//!
//! Every routed type is asked for through the schema rather than through the
//! router directly: what a client gets back is the id it asked with, and the
//! interface field has to resolve on each implementor for that to be true. The
//! ids themselves are asserted against the field that hands them out, so a
//! minted id and a looked-up id cannot drift apart.

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::super::mutation::Mutation;
use super::super::query::Query;
use super::{mcp_server_id, script_id, yolo_profile_id};
use crate::commands::serve::testutil::{state_with_agent_paths, with_home};
use crate::commands::serve::types::AppState;
use crate::runstate::{RunMeta, create_run};

/// Run one query against a state, and refuse to read an answer that failed.
async fn ask(state: AppState, query: &str) -> serde_json::Value {
    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// The same, over a state this test built for itself.
async fn ask_fresh(query: &str) -> serde_json::Value {
    ask(state_with_agent_paths(Vec::new()), query).await
}

/// One `.rhai` tool in a directory, written the way the scripts walk reads it.
fn write_tool(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).expect("the tools directory");
    std::fs::write(
        dir.join(format!("{name}.rhai")),
        format!("// @tool {name}\n// @description does a thing\n\"ok\""),
    )
    .expect("a tool");
}

/// The tags are what make an id routable, so they are what a test asserts.
#[test]
fn a_tagged_id_carries_its_kind_and_its_whole_key() {
    assert_eq!(mcp_server_id("docs").as_str(), "mcpServer:docs");
    assert_eq!(yolo_profile_id("careful").as_str(), "yoloProfile:careful");
    assert_eq!(
        script_id("tool", None, "summarise").as_str(),
        "script:tool:summarise"
    );
    // The blueprint hangs off the kind, so the name stays the whole tail and
    // the `/` a script in a subdirectory has in it.
    assert_eq!(
        script_id("tool", Some("coder"), "text/redact").as_str(),
        "script:tool@coder:text/redact"
    );
}

/// A run answers to the id every other surface calls it by.
#[tokio::test]
async fn a_run_answers_to_its_run_id() {
    crate::runstate::with_isolated_runs_dir_async("graphql-node-run", |_d| async move {
        let meta = RunMeta::new(
            "coder-1788924523-abc123".to_string(),
            "coder".to_string(),
            "/agents/coder".to_string(),
            "do the thing".to_string(),
            None,
            "/work".to_string(),
            1,
        );
        create_run(&meta).expect("run written");

        let json = ask_fresh(
            r#"{ node(id: "coder-1788924523-abc123") { id ... on RunOutput { blueprintName } } }"#,
        )
        .await;
        assert_eq!(json["node"]["id"], "coder-1788924523-abc123");
        assert_eq!(json["node"]["blueprintName"], "coder");

        // A run that was deleted, and a run that never was, read the same.
        let gone = ask_fresh(r#"{ node(id: "coder-1788924523-000000") { id } }"#).await;
        assert!(gone["node"].is_null());
    })
    .await;
}

/// A blueprint answers to the revision id the listing handed out, and to no
/// other digest.
#[tokio::test]
async fn a_blueprint_answers_to_the_revision_id_it_published() {
    with_home(|_home| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        let dir = agents.path().join("alpha");
        std::fs::create_dir_all(&dir).expect("agent dir");
        std::fs::write(
            dir.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"alpha\"\nversion = \"1.0.0\"\n",
        )
        .expect("manifest written");
        let state = state_with_agent_paths(vec![agents.path().to_path_buf()]);

        let listed = ask(state.clone(), "{ blueprints { results { id } } }").await;
        let id = listed["blueprints"]["results"][0]["id"]
            .as_str()
            .expect("a revision id")
            .to_string();

        let json = ask(
            state.clone(),
            &format!("{{ node(id: \"{id}\") {{ id ... on BlueprintOutput {{ name version }} }} }}"),
        )
        .await;
        assert_eq!(json["node"]["id"], id);
        assert_eq!(json["node"]["name"], "alpha");
        assert_eq!(json["node"]["version"], "1.0.0");

        // Another revision of the same name is another node, and this machine
        // does not have it.
        let other = ask(
            state.clone(),
            r#"{ node(id: "alpha@000000000000") { id } }"#,
        )
        .await;
        assert!(other["node"].is_null(), "the digest is part of the id");

        // A name nothing is installed under.
        let ghost = ask(state, r#"{ node(id: "ghost@000000000000") { id } }"#).await;
        assert!(ghost["node"].is_null());
    })
    .await;
}

/// A global script and one blueprint's own are two nodes, and each answers to
/// its own id.
#[tokio::test]
async fn a_script_answers_to_its_kind_its_blueprint_and_its_name() {
    with_home(|home| async move {
        write_tool(&home.join(".leviath").join("tools"), "summarise");
        write_tool(
            &home
                .join(".leviath")
                .join("agents")
                .join("coder")
                .join("tools"),
            "summarise",
        );

        let global = ask_fresh(
            r#"{ node(id: "script:tool:summarise") { id
                 ... on ScriptOutput { kind name scope blueprintName } } }"#,
        )
        .await;
        assert_eq!(global["node"]["id"], "script:tool:summarise");
        assert_eq!(global["node"]["kind"], "TOOL");
        assert_eq!(global["node"]["scope"], "GLOBAL");
        assert!(global["node"]["blueprintName"].is_null());

        let scoped = ask_fresh(
            r#"{ node(id: "script:tool@coder:summarise") { id
                 ... on ScriptOutput { scope blueprintName } } }"#,
        )
        .await;
        assert_eq!(scoped["node"]["id"], "script:tool@coder:summarise");
        assert_eq!(scoped["node"]["scope"], "BLUEPRINT");
        assert_eq!(scoped["node"]["blueprintName"], "coder");

        // The listing and the lookup agree on the spelling. Several ids at
        // once, because `nodes` is what a client holding a page of them asks
        // with: one answer per id, in the order it asked.
        let both = ask_fresh(
            r#"{ nodes(ids: ["script:tool@coder:summarise", "script:tool:summarise",
                 "script:tool:ghost"]) { id } }"#,
        )
        .await;
        let answers = both["nodes"].as_array().expect("one per id");
        assert_eq!(answers.len(), 3, "{answers:?}");
        assert_eq!(answers[0]["id"], "script:tool@coder:summarise");
        assert_eq!(answers[1]["id"], "script:tool:summarise");
        assert!(answers[2].is_null(), "a dead id costs only its own slot");

        // A kind with no name after it, a name nothing is filed under, and a
        // blueprint name no script can be filed under: absences, not failures.
        for id in [
            "script:tool",
            "script:tool:ghost",
            "script:hook@coder:summarise",
            "script:tool@../../etc:summarise",
        ] {
            let json = ask_fresh(&format!("{{ node(id: \"{id}\") {{ id }} }}")).await;
            assert!(json["node"].is_null(), "{id} names nothing");
        }
    })
    .await;
}

/// An MCP server answers to its name, tagged.
#[tokio::test]
async fn an_mcp_server_answers_to_its_name() {
    with_home(|home| async move {
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
                let json = ask_fresh(
                    r#"{ node(id: "mcpServer:docs") { id ... on McpServerOutput { name endpoint } } }"#,
                )
                .await;
                assert_eq!(json["node"]["id"], "mcpServer:docs");
                assert_eq!(json["node"]["name"], "docs");
                assert_eq!(json["node"]["endpoint"], "docs-mcp");

                let listed = ask_fresh("{ mcpServers { results { id } } }").await;
                assert_eq!(listed["mcpServers"]["results"][0]["id"], "mcpServer:docs");

                let ghost = ask_fresh(r#"{ node(id: "mcpServer:ghost") { id } }"#).await;
                assert!(ghost["node"].is_null());
            })
            .await;
    })
    .await;
}

/// A config file that will not parse fails the field rather than answering
/// null, which is what `mcpServers` does with the same file.
///
/// The one place `node` does not report an absence: nothing here was asked and
/// answered "not there", and a null would read as a server somebody deleted.
#[tokio::test]
async fn an_mcp_server_in_an_unreadable_config_is_a_failure_not_an_absence() {
    with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "[[mcp_servers]\nname = broken").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
                    .data(state_with_agent_paths(Vec::new()))
                    .finish();
                let answer = schema
                    .execute(Request::new(r#"{ node(id: "mcpServer:docs") { id } }"#))
                    .await;
                let failure = answer.errors.first().expect("the config failure");
                assert!(
                    failure.message.contains("TOML parse error"),
                    "{}",
                    failure.message
                );

                // The batch lookup and the field answer the same way: one bad
                // id in a list fails the list rather than leaving a null in it.
                let many = schema
                    .execute(Request::new(r#"{ nodes(ids: ["mcpServer:docs"]) { id } }"#))
                    .await;
                assert!(
                    many.errors
                        .first()
                        .is_some_and(|error| error.message.contains("TOML parse error")),
                    "{:?}",
                    many.errors
                );
                let field = schema
                    .execute(Request::new(r#"{ mcpServer(name: "docs") { name } }"#))
                    .await;
                assert!(
                    field
                        .errors
                        .first()
                        .is_some_and(|error| error.message.contains("TOML parse error")),
                    "{:?}",
                    field.errors
                );
            })
            .await;
    })
    .await;
}

/// A yolo profile answers to its name, tagged.
#[tokio::test]
async fn a_yolo_profile_answers_to_its_name() {
    with_home(|_home| async move {
        let path = crate::yolo::yolo_path();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, crate::commands::yolo::EXAMPLE_TOML).expect("the profiles");

        let json = ask_fresh(
            r#"{ node(id: "yoloProfile:careful") { id ... on YoloProfileOutput { name default } } }"#,
        )
        .await;
        assert_eq!(json["node"]["id"], "yoloProfile:careful");
        assert_eq!(json["node"]["name"], "careful");

        let listed = ask_fresh("{ yoloProfiles { results { id name } } }").await;
        let first = &listed["yoloProfiles"]["results"][0];
        assert_eq!(
            first["id"],
            format!("yoloProfile:{}", first["name"].as_str().expect("a name"))
        );

        let ghost = ask_fresh(r#"{ node(id: "yoloProfile:ghost") { id } }"#).await;
        assert!(ghost["node"].is_null());
    })
    .await;
}

/// An update this server ran answers to its job id.
#[tokio::test]
async fn an_update_job_answers_to_its_job_id() {
    with_home(|_home| async move {
        let state = state_with_agent_paths(Vec::new());
        let job = state.update_jobs.start().expect("nothing else is running");

        let json = ask(
            state.clone(),
            &format!(
                "{{ node(id: \"{}\") {{ id ... on UpdateJobOutput {{ status }} }} }}",
                job.id
            ),
        )
        .await;
        assert_eq!(json["node"]["id"], job.id);
        assert_eq!(json["node"]["status"], "RUNNING");

        let ghost = ask(state, r#"{ node(id: "update-1-1") { id } }"#).await;
        assert!(ghost["node"].is_null());
    })
    .await;
}

/// An export this server started answers to its job id.
#[tokio::test]
async fn an_export_answers_to_its_job_id() {
    crate::runstate::with_isolated_runs_dir_async("graphql-node-export", |_d| async move {
        let state = state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();
        let started = schema
            .execute(Request::new(
                "mutation { startRunExport(request: {}) { export { id } } }",
            ))
            .await;
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        let json = serde_json::to_value(&started.data).expect("data serializes");
        let id = json["startRunExport"]["export"]["id"]
            .as_str()
            .expect("an id")
            .to_string();

        let found = schema
            .execute(Request::new(format!(
                "{{ node(id: \"{id}\") {{ id ... on RunExportOutput {{ status }} }} }}"
            )))
            .await;
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let json = serde_json::to_value(&found.data).expect("data serializes");
        assert_eq!(json["node"]["id"], id);

        // An export that expired and one that never existed read the same.
        let gone = schema
            .execute(Request::new(r#"{ node(id: "export-1-1") { id } }"#))
            .await;
        assert!(gone.errors.is_empty(), "{:?}", gone.errors);
        let json = serde_json::to_value(&gone.data).expect("data serializes");
        assert!(json["node"].is_null());
    })
    .await;
}

/// A batch lookup is bounded by the same cap the run listing puts on a named
/// list of ids, because it is the same fan-out: one read per id.
#[tokio::test]
async fn a_batch_lookup_is_capped_at_the_same_number_of_ids_the_listing_allows() {
    crate::runstate::with_isolated_runs_dir_async("graphql-node-cap", |_d| async move {
        let cap = crate::commands::serve::core::runs::MAX_IDS;
        let listed = |count: usize| {
            (0..count)
                .map(|at| format!("\"thing:{at}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };

        let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
            .data(state_with_agent_paths(Vec::new()))
            .finish();
        let too_many = schema
            .execute(Request::new(format!(
                "{{ nodes(ids: [{}]) {{ id }} }}",
                listed(cap + 1)
            )))
            .await;
        let refusal = too_many.errors.first().expect("a refusal");
        assert!(refusal.message.contains("at most"), "{}", refusal.message);
        assert_eq!(
            refusal
                .extensions
                .as_ref()
                .and_then(|ext| ext.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );

        // The cap itself is an answer, not a refusal.
        let json = ask_fresh(&format!("{{ nodes(ids: [{}]) {{ id }} }}", listed(cap))).await;
        assert_eq!(json["nodes"].as_array().map(Vec::len), Some(cap));
    })
    .await;
}

/// An id this schema has no type for is an absence, not a failure.
#[tokio::test]
async fn an_id_that_routes_nowhere_answers_null() {
    crate::runstate::with_isolated_runs_dir_async("graphql-node-nowhere", |_d| async move {
        for id in ["thing:1", "", "not-an-id-at-all"] {
            let json = ask_fresh(&format!("{{ node(id: \"{id}\") {{ id }} }}")).await;
            assert!(json["node"].is_null(), "'{id}' routes nowhere");
        }
    })
    .await;
}
