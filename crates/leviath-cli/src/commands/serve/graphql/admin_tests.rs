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
        r#"mutation { putMimeRow(row: { mimeType: "image/png" }) { created } }"#,
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
                r#"mutation { putMimeRow(row: { mimeType: "application/x-thing", family: "binary",
                     extensions: ["thing"] }) { mimeType created } }"#,
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
                r#"mutation { putMimeRow(row: { mimeType: "application/x-thing", family: "document" })
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
                r#"mutation { putMimeRow(row: { mimeType: "not a mime type" }) { created } }"#,
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

/// Every admin mutation added since the first batch is behind the same gate.
///
/// One list, checked as a whole: a mutation added without its guard is invisible
/// to every other test, and this is the one that would catch it.
#[tokio::test]
async fn every_machine_changing_mutation_is_gated() {
    let calls = [
        r#"mutation { updateConfig(input: { defaultProvider: "openai" }) { defaultProvider } }"#,
        r#"mutation { putScript(kind: "tool", name: "x", content: "fn x(){}") { path } }"#,
        r#"mutation { deleteScript(kind: "tool", name: "x") }"#,
        r#"mutation { makeDirectory(path: "/tmp", name: "x") { path } }"#,
        r#"mutation { runDoctorLive { ok } }"#,
        r#"mutation { startUpdate { id } }"#,
    ];
    for call in calls {
        let answer = schema(false).execute(Request::new(call)).await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("--allow-admin"),
            "{call} is gated: {}",
            error.message
        );
    }
}

/// A mime row can be written whole, with its token rule, and the rule is checked
/// the same way the REST route checks it.
#[tokio::test]
async fn a_mime_row_carries_its_token_rule() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = schema(true)
            .execute(Request::new(
                r#"mutation { putMimeRow(row: { mimeType: "image/x-thing", family: "image",
                     extensions: ["thing"], magic: "89504e47", standIn: "[a thing]",
                     tokens: { perPixel: 750, max: 1600 } }) { mimeType created } }"#,
            ))
            .await;
        assert!(written.errors.is_empty(), "{:?}", written.errors);

        let listed = schema(true)
            .execute(Request::new("{ mime { mimeType family extensions } }"))
            .await;
        let json = serde_json::to_value(&listed.data).expect("data serializes");
        assert!(
            json["mime"]
                .as_array()
                .expect("rows")
                .iter()
                .any(|row| row["mimeType"] == "image/x-thing"),
            "the row is in the registry"
        );

        // Two rates at once is not a rule. Refused here rather than saved as
        // whichever one the reader happened to check first.
        let refused = schema(true)
            .execute(Request::new(
                r#"mutation { putMimeRow(row: { mimeType: "image/x-two",
                     tokens: { perPixel: 750, perSecond: 4 } }) { created } }"#,
            ))
            .await;
        assert_eq!(
            refused
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );
        // And `max` without `perPixel` is a ceiling on nothing.
        let refused = schema(true)
            .execute(Request::new(
                r#"mutation { putMimeRow(row: { mimeType: "image/x-three",
                     tokens: { perByte: 0.25, max: 10 } }) { created } }"#,
            ))
            .await;
        assert!(!refused.errors.is_empty(), "max only goes with perPixel");
    })
    .await;
}

/// A config write is a partial edit with three states per setting, and the
/// answer is the config as it now stands.
#[tokio::test]
async fn a_config_write_sets_clears_and_leaves_alone() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "default_provider = \"anthropic\"\n").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let set = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: {
                             defaultProvider: "openai",
                             overrideModel: "gpt-5.6",
                             providerOrder: ["openai", "anthropic"]
                           }) { defaultProvider overrideModel providerOrder } }"#,
                    ))
                    .await;
                assert!(set.errors.is_empty(), "{:?}", set.errors);
                let json = serde_json::to_value(&set.data).expect("data serializes");
                assert_eq!(json["updateConfig"]["defaultProvider"], "openai");
                assert_eq!(json["updateConfig"]["overrideModel"], "gpt-5.6");
                assert_eq!(json["updateConfig"]["providerOrder"][0], "openai");

                // A field left out leaves the setting alone, and null clears it:
                // two different things one nullable field could not tell apart.
                let cleared = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: { overrideModel: null })
                             { defaultProvider overrideModel } }"#,
                    ))
                    .await;
                assert!(cleared.errors.is_empty(), "{:?}", cleared.errors);
                let json = serde_json::to_value(&cleared.data).expect("data serializes");
                assert!(json["updateConfig"]["overrideModel"].is_null(), "cleared");
                assert_eq!(
                    json["updateConfig"]["defaultProvider"], "openai",
                    "and the field nobody sent is untouched"
                );

                // An empty string is refused rather than read as a clear.
                let refused = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: { overrideModel: "" })
                             { overrideModel } }"#,
                    ))
                    .await;
                let error = refused.errors.first().expect("a refusal");
                assert!(
                    error.message.contains("send null to clear it"),
                    "{}",
                    error.message
                );
            })
            .await;
    })
    .await;
}

/// A script is written, read back through the schema, and removed.
#[tokio::test]
async fn a_script_can_be_written_and_removed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = schema(true)
            .execute(Request::new(
                r#"mutation { putScript(kind: "tool", name: "greet",
                     content: "fn describe() { #{ name: \"greet\", description: \"hi\" } }")
                     { path compiles error } }"#,
            ))
            .await;
        assert!(written.errors.is_empty(), "{:?}", written.errors);
        let json = serde_json::to_value(&written.data).expect("data serializes");
        assert!(json["putScript"]["path"].as_str().is_some());

        // A script that does not compile is still written: an editor saves work
        // in progress, and the run is what refuses to use it.
        let broken = schema(true)
            .execute(Request::new(
                r#"mutation { putScript(kind: "tool", name: "broken", content: "fn (")
                     { compiles error } }"#,
            ))
            .await;
        assert!(broken.errors.is_empty(), "{:?}", broken.errors);
        let json = serde_json::to_value(&broken.data).expect("data serializes");
        assert_eq!(json["putScript"]["compiles"], false);
        assert!(json["putScript"]["error"].as_str().is_some(), "it says why");

        let removed = schema(true)
            .execute(Request::new(r#"mutation { deleteScript(kind: "tool", name: "greet") }"#))
            .await;
        assert!(removed.errors.is_empty(), "{:?}", removed.errors);

        // And one that is not there is a miss rather than a silent success.
        let gone = schema(true)
            .execute(Request::new(r#"mutation { deleteScript(kind: "tool", name: "greet") }"#))
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

        // An unknown registry is refused before any path is built.
        let unknown = schema(true)
            .execute(Request::new(
                r#"mutation { putScript(kind: "model_provider", name: "x", content: "") { path } }"#,
            ))
            .await;
        assert!(
            unknown
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("Unknown script kind")
        );
    })
    .await;
}

/// Making a directory tells its three refusals apart, because a picker shows
/// each of them differently.
#[tokio::test]
async fn making_a_directory_tells_its_refusals_apart() {
    let dir = tempfile::tempdir().expect("a directory");
    let parent = dir.path().to_string_lossy().into_owned();

    let made = schema(true)
        .execute(Request::new(format!(
            r#"mutation {{ makeDirectory(path: "{parent}", name: "new-thing")
                 {{ path parent }} }}"#
        )))
        .await;
    assert!(made.errors.is_empty(), "{:?}", made.errors);
    let json = serde_json::to_value(&made.data).expect("data serializes");
    assert!(
        json["makeDirectory"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("new-thing"))
    );

    let code = |answer: &async_graphql::Response| -> String {
        answer
            .errors
            .first()
            .expect("a refusal")
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .map(ToString::to_string)
            .unwrap_or_default()
    };

    // Already there.
    let again = schema(true)
        .execute(Request::new(format!(
            r#"mutation {{ makeDirectory(path: "{parent}", name: "new-thing") {{ path }} }}"#
        )))
        .await;
    assert_eq!(code(&again), "\"CONFLICT\"");

    // A name that is a path is not a name.
    let nested = schema(true)
        .execute(Request::new(format!(
            r#"mutation {{ makeDirectory(path: "{parent}", name: "a/b") {{ path }} }}"#
        )))
        .await;
    assert_eq!(code(&nested), "\"BAD_USER_INPUT\"");

    // A parent that is not there.
    let missing = schema(true)
        .execute(Request::new(format!(
            r#"mutation {{ makeDirectory(path: "{parent}/nope", name: "x") {{ path }} }}"#
        )))
        .await;
    assert_eq!(code(&missing), "\"NOT_FOUND\"");

    // And a relative path, which this route never resolves for the caller.
    let relative = schema(true)
        .execute(Request::new(
            r#"mutation { makeDirectory(path: "somewhere", name: "x") { path } }"#,
        ))
        .await;
    assert_eq!(code(&relative), "\"BAD_USER_INPUT\"");
}

/// The mutations that reach outside this machine are behind the gate too.
#[tokio::test]
async fn the_outward_reaching_mutations_are_gated() {
    let calls = [
        r#"mutation { providerSignIn(provider: "anthropic") { authorizeUrl } }"#,
        r#"mutation { providerSignOut(provider: "anthropic") }"#,
        r#"mutation { checkProvider(provider: "anthropic") }"#,
        r#"mutation { testMcpServer(name: "docs") }"#,
        r#"mutation { loginMcpServer(name: "docs") }"#,
        r#"mutation { probeModels(baseUrl: "http://127.0.0.1:1") }"#,
        r#"mutation { putYoloProfiles(text: "") { path } }"#,
    ];
    for call in calls {
        let answer = schema(false).execute(Request::new(call)).await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("--allow-admin"),
            "{call} is gated: {}",
            error.message
        );
    }
}

/// A provider nobody can sign in to in a browser is a miss, named as such.
///
/// Told apart from a provider that exists and refused: one is a client using the
/// wrong name, the other is something to retry.
#[tokio::test]
async fn an_unknown_signin_provider_is_a_miss() {
    for call in [
        r#"mutation { providerSignIn(provider: "nope") { authorizeUrl } }"#,
        r#"mutation { providerSignOut(provider: "nope") }"#,
        r#"mutation { checkProvider(provider: "nope") }"#,
    ] {
        let answer = schema(true).execute(Request::new(call)).await;
        let error = answer.errors.first().expect("a refusal");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string()),
            "{call}"
        );
        assert!(
            error.message.contains("browser sign-in"),
            "it says what kind of name it wanted: {}",
            error.message
        );
    }
}

/// An MCP server that is not in the config cannot be tested or signed in to.
#[tokio::test]
async fn an_unknown_mcp_server_cannot_be_tested() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                for call in [
                    r#"mutation { testMcpServer(name: "nope") }"#,
                    r#"mutation { loginMcpServer(name: "nope") }"#,
                ] {
                    let answer = schema(true).execute(Request::new(call)).await;
                    assert_eq!(
                        answer
                            .errors
                            .first()
                            .expect("a refusal")
                            .extensions
                            .as_ref()
                            .and_then(|e| e.get("code"))
                            .map(ToString::to_string),
                        Some("\"NOT_FOUND\"".to_string()),
                        "{call}"
                    );
                }
            })
            .await;
    })
    .await;
}

/// A probe of an address that is not a URL is refused before anything is dialled.
#[tokio::test]
async fn a_probe_of_something_that_is_not_a_url_is_refused() {
    let answer = schema(true)
        .execute(Request::new(
            r#"mutation { probeModels(baseUrl: "not a url") }"#,
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
}

/// The yolo file is written whole, and a file that would not load is refused
/// rather than saved and discovered at the next spawn.
#[tokio::test]
async fn the_yolo_file_is_written_whole_and_parse_checked() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = schema(true)
            .execute(Request::new(format!(
                r#"mutation {{ putYoloProfiles(text: {}) {{ path exists error
                     profiles {{ name default }} }} }}"#,
                serde_json::json!(crate::commands::yolo::EXAMPLE_TOML)
            )))
            .await;
        assert!(written.errors.is_empty(), "{:?}", written.errors);
        let json = serde_json::to_value(&written.data).expect("data serializes");
        assert_eq!(json["putYoloProfiles"]["exists"], true);
        assert!(json["putYoloProfiles"]["error"].is_null());
        assert!(
            !json["putYoloProfiles"]["profiles"]
                .as_array()
                .expect("profiles")
                .is_empty(),
            "the file it just wrote is read back"
        );

        let refused = schema(true)
            .execute(Request::new(
                r#"mutation { putYoloProfiles(text: "[[[ not toml") { path } }"#,
            ))
            .await;
        assert_eq!(
            refused
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
}
