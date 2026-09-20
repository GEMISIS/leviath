//! Tests for the admin gate.
//!
//! Two properties, and they are different: a server without `--allow-admin`
//! does not show these fields to introspection, and refuses them when asked
//! anyway. The second is the one that matters; the first is so a client
//! exploring the schema is not offered acts that will be refused.

use async_graphql::{EmptySubscription, Request, Schema};

use crate::commands::serve::graphql::mutation::Mutation;
use crate::commands::serve::graphql::query::Query;

/// A schema built for a server with or without the flag.
fn schema(allow_admin: bool) -> Schema<Query, Mutation, EmptySubscription> {
    let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .data(super::AdminAccess(allow_admin))
        .finish()
}

/// The admin fields are hidden from introspection without the flag, and shown
/// with it.
#[tokio::test]
async fn the_admin_fields_are_hidden_without_the_flag() {
    let query = "{ __type(name: \"Mutation\") { fields { name } } }";

    let closed = schema(false).execute(Request::new(query)).await;
    let json = serde_json::to_value(&closed.data).expect("data serializes");
    let names: Vec<String> = json["__type"]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|field| field["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.iter().any(|name| name == "spawnAgent"),
        "the ordinary mutations are there: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "addMcpServer"),
        "and the admin ones are not: {names:?}"
    );

    let open = schema(true).execute(Request::new(query)).await;
    let json = serde_json::to_value(&open.data).expect("data serializes");
    let names: Vec<String> = json["__type"]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|field| field["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.iter().any(|name| name == "addMcpServer"),
        "with the flag they are: {names:?}"
    );
}

/// Hidden is not the boundary: a client that knows the field name is refused,
/// with the code to branch on and the flag named in the message.
#[tokio::test]
async fn an_admin_mutation_is_refused_without_the_flag() {
    let answer = schema(false)
        .execute(Request::new(
            r#"mutation { addMcpServer(name: "x", command: "/bin/echo") }"#,
        ))
        .await;
    let error = answer.errors.first().expect("a refusal");
    assert!(
        error.message.contains("--allow-admin"),
        "it names the flag: {}",
        error.message
    );
    assert_eq!(
        error
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .map(ToString::to_string),
        Some("\"FORBIDDEN\"".to_string())
    );
}

/// Every admin mutation is behind the same gate, not just the first one.
#[tokio::test]
async fn every_admin_mutation_is_gated() {
    let calls = [
        r#"mutation { addMcpServer(name: "x", command: "/bin/echo") }"#,
        r#"mutation { removeMcpServer(name: "x") }"#,
        r#"mutation { putMimeRow(mimeType: "image/png") { created } }"#,
        r#"mutation { deleteMimeRow(mimeType: "image/png") }"#,
    ];
    for call in calls {
        let answer = schema(false).execute(Request::new(call)).await;
        assert!(
            answer
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("--allow-admin"),
            "{call} is gated"
        );
    }
}

/// With the flag, an MCP server can be added and taken away again.
#[tokio::test]
async fn an_mcp_server_can_be_added_and_removed() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS.scope(paths, async {
            let added = schema(true)
                .execute(Request::new(
                    r#"mutation { addMcpServer(name: "docs", command: "/bin/echo", args: ["hi"]) }"#,
                ))
                .await;
            assert!(added.errors.is_empty(), "{:?}", added.errors);

            let listed = schema(true)
                .execute(Request::new("{ mcpServers { name transport endpoint } }"))
                .await;
            let json = serde_json::to_value(&listed.data).expect("data serializes");
            assert_eq!(json["mcpServers"][0]["name"], "docs");
            assert_eq!(json["mcpServers"][0]["transport"], "stdio");

            // The same name twice is a conflict: the second would replace a
            // command the operator already approved.
            let again = schema(true)
                .execute(Request::new(
                    r#"mutation { addMcpServer(name: "docs", command: "/bin/echo") }"#,
                ))
                .await;
            assert_eq!(
                again
                    .errors
                    .first()
                    .expect("a refusal")
                    .extensions
                    .as_ref()
                    .and_then(|e| e.get("code"))
                    .map(ToString::to_string),
                Some("\"CONFLICT\"".to_string())
            );

            let removed = schema(true)
                .execute(Request::new(r#"mutation { removeMcpServer(name: "docs") }"#))
                .await;
            assert!(removed.errors.is_empty(), "{:?}", removed.errors);

            let gone = schema(true)
                .execute(Request::new(r#"mutation { removeMcpServer(name: "docs") }"#))
                .await;
            assert_eq!(
                gone.errors
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
    })
    .await;
}

/// A server with a command that will not validate is refused before anything
/// is written.
#[tokio::test]
async fn a_server_that_will_not_validate_is_refused() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                // Neither a command nor a URL: there is nothing to reach.
                let answer = schema(true)
                    .execute(Request::new(r#"mutation { addMcpServer(name: "empty") }"#))
                    .await;
                assert_eq!(
                    answer
                        .errors
                        .first()
                        .expect("a refusal")
                        .extensions
                        .as_ref()
                        .and_then(|e| e.get("code"))
                        .map(ToString::to_string),
                    Some("\"BAD_USER_INPUT\"".to_string())
                );
            })
            .await;
    })
    .await;
}

/// A mime row is written, then updated, then removed.
#[tokio::test]
async fn a_mime_row_can_be_written_and_removed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = schema(true)
            .execute(Request::new(
                r#"mutation { putMimeRow(mimeType: "application/x-thing", family: "binary",
                     extensions: ["thing"]) { mimeType created } }"#,
            ))
            .await;
        assert!(written.errors.is_empty(), "{:?}", written.errors);
        let json = serde_json::to_value(&written.data).expect("data serializes");
        assert_eq!(json["putMimeRow"]["mimeType"], "application/x-thing");
        assert_eq!(json["putMimeRow"]["created"], true);

        // The same row again updates rather than creates, which is what the
        // flag on the way back is for.
        let updated = schema(true)
            .execute(Request::new(
                r#"mutation { putMimeRow(mimeType: "application/x-thing", family: "document")
                     { created } }"#,
            ))
            .await;
        let json = serde_json::to_value(&updated.data).expect("data serializes");
        assert_eq!(json["putMimeRow"]["created"], false);

        let removed = schema(true)
            .execute(Request::new(
                r#"mutation { deleteMimeRow(mimeType: "application/x-thing") }"#,
            ))
            .await;
        let json = serde_json::to_value(&removed.data).expect("data serializes");
        assert_eq!(json["deleteMimeRow"], true);

        // Removing one that is not there is false rather than an error: that
        // is a fact about the registry, not a failed request.
        let again = schema(true)
            .execute(Request::new(
                r#"mutation { deleteMimeRow(mimeType: "application/x-thing") }"#,
            ))
            .await;
        let json = serde_json::to_value(&again.data).expect("data serializes");
        assert_eq!(json["deleteMimeRow"], false);
    })
    .await;
}

/// A mime type that is not a mime type is refused.
#[tokio::test]
async fn a_row_key_that_is_not_a_mime_type_is_refused() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = schema(true)
            .execute(Request::new(
                r#"mutation { putMimeRow(mimeType: "not a mime type") { created } }"#,
            ))
            .await;
        assert_eq!(
            answer
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );

        let deleting = schema(true)
            .execute(Request::new(
                r#"mutation { deleteMimeRow(mimeType: "not a mime type") }"#,
            ))
            .await;
        assert!(!deleting.errors.is_empty(), "refused on the way out too");
    })
    .await;
}
