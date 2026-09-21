//! Tests for the admin gate.
//!
//! Two properties, and they are different: a server without `--allow-admin`
//! does not show these fields to introspection, and refuses them when asked
//! anyway. The second is the one that matters; the first is so a client
//! exploring the schema is not offered acts that will be refused.

use async_graphql::{EmptySubscription, Request, Schema};

use super::{MimeRowInput, MimeTokensInput};
use crate::commands::serve::graphql::mutation::Mutation;
use crate::commands::serve::graphql::query::Query;

/// The `extensions.code` of a refusal, as the schema sends it.
fn refusal_code(answer: &async_graphql::Response) -> String {
    answer
        .errors
        .first()
        .expect("a refusal")
        .extensions
        .as_ref()
        .and_then(|e| e.get("code"))
        .map(ToString::to_string)
        .unwrap_or_default()
}

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
        names.iter().any(|name| name == "spawnRun"),
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

/// An HTTP server added with an `Authorization` header is a server with a
/// credential, so it is not offered a sign-in.
///
/// The headers were the difference between the two surfaces: a server that
/// needs a header could be added over REST and not here, which meant adding it
/// over GraphQL produced one that could never answer.
#[tokio::test]
async fn an_http_server_can_be_added_with_its_headers() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let added = schema(true)
                    .execute(Request::new(
                        r#"mutation { addMcpServer(name: "docs", url: "https://docs.example/mcp",
                             headers: [{ name: "Authorization", value: "Bearer t" }]) }"#,
                    ))
                    .await;
                assert!(added.errors.is_empty(), "{:?}", added.errors);

                let listed = schema(true)
                    .execute(Request::new(
                        "{ mcpServers { name transport endpoint auth configError } }",
                    ))
                    .await;
                let json = serde_json::to_value(&listed.data).expect("data serializes");
                let server = &json["mcpServers"][0];
                assert_eq!(server["transport"], "HTTP");
                assert_eq!(server["endpoint"], "https://docs.example/mcp");
                assert_eq!(
                    server["auth"], "HEADER",
                    "the header is the credential, so no login is offered"
                );
                assert!(server["configError"].is_null());
            })
            .await;
    })
    .await;
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
            assert_eq!(json["mcpServers"][0]["transport"], "STDIO");

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
                     content: "// @tool greet\n// @description says hello\n\"hi\"")
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

    // The path travels as a variable rather than inside the query text. A
    // Windows path is full of backslashes and a backslash escapes inside a
    // GraphQL string, so interpolating one is a parse error on that platform
    // and nowhere else.
    let ask = |query: &str, path: &str, name: &str| {
        Request::new(query).variables(async_graphql::Variables::from_json(
            serde_json::json!({ "path": path, "name": name }),
        ))
    };
    let make = r#"mutation Make($path: String!, $name: String!) {
        makeDirectory(path: $path, name: $name) { path parent }
    }"#;

    let made = schema(true).execute(ask(make, &parent, "new-thing")).await;
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
    let again = schema(true).execute(ask(make, &parent, "new-thing")).await;
    assert_eq!(code(&again), "\"CONFLICT\"");

    // A name that is a path is not a name.
    let nested = schema(true).execute(ask(make, &parent, "a/b")).await;
    assert_eq!(code(&nested), "\"BAD_USER_INPUT\"");

    // A parent that is not there.
    let absent = dir.path().join("nope").to_string_lossy().into_owned();
    let missing = schema(true).execute(ask(make, &absent, "x")).await;
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
        r#"mutation { providerSignIn(provider: "codex") { authorizeUrl } }"#,
        r#"mutation { providerSignOut(provider: "codex") }"#,
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

/// The acts that do real work, driven through the seams the REST tests use.
///
/// Each of these opens a browser, runs a package manager or reaches a provider
/// on a real machine, so a test that let them do that would be a test of the
/// developer's laptop. What is asserted is the answer each one gives once its
/// seam has stood in for the world.
mod doing_the_work {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A schema over a state a test has arranged.
    fn schema_over(
        state: crate::commands::serve::types::AppState,
    ) -> Schema<Query, Mutation, EmptySubscription> {
        Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .data(super::super::AdminAccess(true))
            .finish()
    }

    /// A sign-in answers with the URL as soon as there is one, and the second
    /// ask answers the same URL rather than starting another.
    #[tokio::test]
    async fn a_sign_in_answers_with_its_url_and_says_when_it_is_the_same_one() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let opened: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&opened);
            let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
            state.providers = crate::commands::serve::providers::ProviderAdmin {
                // Nothing is really opened: the flow announces the URL, and that
                // is what the mutation waits for.
                opener: Arc::new(move |url: &str| {
                    leviath_core::sync::lock(&seen).push(url.to_string());
                    true
                }),
                // An issuer nothing listens on: the announce happens before the
                // exchange, so the URL is answered and the flow then fails
                // quietly behind it.
                issuer: Some("http://127.0.0.1:1".to_string()),
                ports: Some(vec![0]),
                ..Default::default()
            };
            let paths = crate::commands::serve::mcp::AdminPaths {
                config: home.join("config.toml"),
                store: home.join("mcp-auth.json"),
                grants: home.join("grants.json"),
            };
            std::fs::write(&paths.config, "").expect("a config file");
            let schema = schema_over(state.clone());

            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    let started = schema
                        .execute(Request::new(
                            r#"mutation { providerSignIn(provider: "codex")
                                 { provider authorizeUrl alreadyWaiting } }"#,
                        ))
                        .await;
                    // Either the flow announced a URL or it refused before it got
                    // one; both are answers this schema has to give, and neither
                    // is a panic.
                    match started.errors.first() {
                        None => {
                            let json =
                                serde_json::to_value(&started.data).expect("data serializes");
                            assert_eq!(json["providerSignIn"]["provider"], "codex");
                            assert_eq!(json["providerSignIn"]["alreadyWaiting"], false);
                            assert!(
                                json["providerSignIn"]["authorizeUrl"]
                                    .as_str()
                                    .is_some_and(|url| !url.is_empty())
                            );

                            // Asking again while one is waiting answers the same
                            // URL, flagged, rather than starting a second flow
                            // that could not bind the port anyway.
                            let again = schema
                                .execute(Request::new(
                                    r#"mutation { providerSignIn(provider: "codex")
                                         { authorizeUrl alreadyWaiting } }"#,
                                ))
                                .await;
                            assert!(again.errors.is_empty(), "{:?}", again.errors);
                            let repeat =
                                serde_json::to_value(&again.data).expect("data serializes");
                            assert_eq!(repeat["providerSignIn"]["alreadyWaiting"], true);
                            assert_eq!(
                                repeat["providerSignIn"]["authorizeUrl"],
                                json["providerSignIn"]["authorizeUrl"]
                            );
                        }
                        Some(error) => {
                            // Two refusals are possible here and both are
                            // answers rather than panics: this build may not
                            // offer a browser sign-in for this provider at all,
                            // and a flow that never reached a URL is an upstream
                            // failure.
                            let code = error
                                .extensions
                                .as_ref()
                                .and_then(|e| e.get("code"))
                                .map(ToString::to_string)
                                .unwrap_or_default();
                            assert!(
                                code == "\"NOT_FOUND\"" || code == "\"UPSTREAM\"",
                                "unexpected refusal {code}: {}",
                                error.message
                            );
                        }
                    }

                    // Signing out forgets whatever is there, including a flow
                    // left waiting, and says so even when there was nothing.
                    let out = schema
                        .execute(Request::new(
                            r#"mutation { providerSignOut(provider: "codex") }"#,
                        ))
                        .await;
                    assert!(out.errors.is_empty(), "{:?}", out.errors);
                })
                .await;
        })
        .await;
    }

    /// An update runs one at a time, and the second ask is a conflict rather
    /// than a second package manager.
    #[tokio::test]
    async fn one_update_runs_at_a_time() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
            // A runner that reports success without running anything, and an
            // environment pointed at this test's own home.
            let agents = home.join("agents");
            state.update_jobs =
                crate::commands::serve::update_job::UpdateJobs::with_env(Arc::new(move || {
                    crate::commands::update::UpdateEnv {
                        agents_dir: agents.clone(),
                        // Nothing is spawned: the test is about what the job records,
                        // not about what a package manager does.
                        runner: Arc::new(|_argv: &[String]| Ok(())),
                        ..crate::commands::update::UpdateEnv::for_planning_offline()
                    }
                }));
            let schema = schema_over(state.clone());

            let started = schema
                .execute(Request::new(
                    "mutation { startUpdate(binary: false, blueprints: false, migrations: false)
                       { id status steps { step status detail } } }",
                ))
                .await;
            assert!(started.errors.is_empty(), "{:?}", started.errors);
            let json = serde_json::to_value(&started.data).expect("data serializes");
            let id = json["startUpdate"]["id"]
                .as_str()
                .expect("an id")
                .to_string();
            // The same rows whatever was asked for, so a client renders one
            // table and reads `SKIPPED` rather than an absence.
            assert_eq!(
                json["startUpdate"]["steps"].as_array().map(Vec::len),
                Some(4)
            );

            // The same job, read back through the field a client polls.
            let polled = schema
                .execute(Request::new(format!(
                    "{{ updateJob(id: \"{id}\") {{ id status }} }}"
                )))
                .await;
            assert!(polled.errors.is_empty(), "{:?}", polled.errors);
            let json = serde_json::to_value(&polled.data).expect("data serializes");
            assert_eq!(json["updateJob"]["id"], id);

            // An id nobody started is null rather than an error.
            let missing = schema
                .execute(Request::new("{ updateJob(id: \"nope\") { id } }"))
                .await;
            assert!(missing.errors.is_empty(), "{:?}", missing.errors);
            let json = serde_json::to_value(&missing.data).expect("data serializes");
            assert!(json["updateJob"].is_null());
        })
        .await;
    }

    /// The live doctor runs its checks and reports what they found, and a
    /// failing check is a report rather than a failed request.
    #[tokio::test]
    async fn the_live_doctor_reports_what_it_found() {
        crate::config::with_isolated_config_path_async("graphql-live-doctor", |_path| async move {
            let state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
            let answer = schema_over(state)
                .execute(Request::new(
                    "mutation { runDoctorLive { ok checks { name ok detail } } }",
                ))
                .await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            let checks = json["runDoctorLive"]["checks"]
                .as_array()
                .expect("the checks");
            assert!(!checks.is_empty(), "it ran something");
            assert!(
                checks
                    .iter()
                    .all(|check| check["name"].as_str().is_some_and(|n| !n.is_empty())),
                "each check says what it checked: {checks:?}"
            );
            // Nothing is configured in this home, so at least one check fails,
            // and that is a report rather than an error.
            assert_eq!(json["runDoctorLive"]["ok"], false);
        })
        .await;
    }
}

/// Every config field a write can set travels into the config file.
///
/// A field the schema takes and the writer drops is a setting that saves and
/// then does nothing, which is the failure mode this whole input object exists
/// to avoid, so each one is asserted rather than sampled.
#[tokio::test]
async fn every_config_field_reaches_the_file() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        std::fs::write(&paths.config, "").expect("a config file");
        let config_path = paths.config.clone();
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let written = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: {
                             defaultProvider: "openai",
                             providerOrder: ["openai"],
                             overrideModel: "gpt-5.6",
                             fallbackModel: "gpt-5.4",
                             anthropicKey: "sk-ant-a",
                             openaiKey: "sk-o",
                             googleKey: "g",
                             openrouterKey: "sk-or-r",
                             bedrockKey: "b",
                             xaiKey: "xai-x",
                             metaKey: "m",
                             bedrockRegion: "us-east-1",
                             ollamaBaseUrl: "http://127.0.0.1:11434",
                             ollamaEnabled: true,
                             codexEnabled: true,
                             grokEnabled: true,
                             fileUploads: true,
                             codexReasoningEffort: "high",
                             codexVerbosity: "low",
                             codexReplayReasoning: true,
                             gateways: [{ name: "local", kind: "openai-compatible",
                                 baseUrl: "http://127.0.0.1:1234/v1", apiKey: "sk-l",
                                 models: ["llama"],
                                 headers: [{ name: "X-Thing", value: "1" }] }]
                           }) { defaultProvider overrideModel fallbackModel
                                configuredProviders gateways { name kind hasApiKey } } }"#,
                    ))
                    .await;
                assert!(written.errors.is_empty(), "{:?}", written.errors);
                let json = serde_json::to_value(&written.data).expect("data serializes");
                let config = &json["updateConfig"];
                assert_eq!(config["defaultProvider"], "openai");
                assert_eq!(config["overrideModel"], "gpt-5.6");
                assert_eq!(config["fallbackModel"], "gpt-5.4");
                let configured = config["configuredProviders"]
                    .as_array()
                    .expect("the providers with keys");
                for expected in ["anthropic", "openai", "google", "openrouter", "xai", "meta"] {
                    assert!(
                        configured.iter().any(|name| name == expected),
                        "{expected} has a key now: {configured:?}"
                    );
                }
                assert_eq!(config["gateways"][0]["name"], "local");
                assert_eq!(config["gateways"][0]["hasApiKey"], true);

                // The file is the record, so the fields with no read-back field
                // are asserted there: a setting the answer does not echo is
                // still a setting that has to be saved.
                let saved = std::fs::read_to_string(&config_path).expect("the config file");
                for expected in ["us-east-1", "127.0.0.1:11434", "high", "llama", "X-Thing"] {
                    assert!(saved.contains(expected), "{expected} was saved: {saved}");
                }

                // A gateway is removed by name, and removals run after the edits
                // so one request that does both does not depend on the order.
                let removed = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: { removeGateways: ["local"] })
                             { gateways { name } } }"#,
                    ))
                    .await;
                assert!(removed.errors.is_empty(), "{:?}", removed.errors);
                let json = serde_json::to_value(&removed.data).expect("data serializes");
                assert_eq!(
                    json["updateConfig"]["gateways"].as_array().map(Vec::len),
                    Some(0)
                );

                // A word the provider does not know is refused rather than
                // saved: it would be saved and then ignored.
                let refused = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: { codexVerbosity: "shouty" })
                             { defaultProvider } }"#,
                    ))
                    .await;
                assert!(
                    refused
                        .errors
                        .first()
                        .expect("a refusal")
                        .message
                        .contains("unknown Codex"),
                    "{:?}",
                    refused.errors
                );

                // And an empty region is refused rather than read as a clear.
                let empty = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(input: { bedrockRegion: "" })
                             { defaultProvider } }"#,
                    ))
                    .await;
                assert!(
                    empty
                        .errors
                        .first()
                        .expect("a refusal")
                        .message
                        .contains("bedrock_region"),
                    "{:?}",
                    empty.errors
                );
            })
            .await;
    })
    .await;
}

/// The acts that reach a real server, against one this test stands up.
///
/// A probe and an MCP login are requests to somebody else, so the somebody else
/// is a listener bound to a loopback port here. That is the only way to assert
/// what a client is told about an endpoint that answers, as against one that is
/// not there.
mod against_a_real_endpoint {
    use super::*;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    /// An OpenAI-compatible endpoint that serves two models.
    async fn models_endpoint() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let base = format!("http://{}/v1", listener.local_addr().expect("an address"));
        let app = Router::new().route(
            "/v1/models",
            get(|| async {
                Json(serde_json::json!({
                    "data": [{ "id": "small" }, { "id": "large" }],
                }))
            }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    /// A probe reports what the endpoint says it serves, sorted.
    #[tokio::test]
    async fn a_probe_reports_what_the_endpoint_serves() {
        let base = models_endpoint().await;
        let answer = schema(true)
            .execute(Request::new(format!(
                r#"mutation {{ probeModels(baseUrl: "{base}",
                     headers: [{{ name: "X-Thing", value: "1" }}]) }}"#
            )))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(
            json["probeModels"],
            serde_json::json!(["large", "small"]),
            "sorted, so a picker's list does not reorder between two asks"
        );
    }

    /// An endpoint nothing is listening on is an upstream failure rather than an
    /// empty list: no models and "could not ask" are different answers.
    #[tokio::test]
    async fn a_probe_of_a_dead_endpoint_is_an_upstream_failure() {
        let answer = schema(true)
            .execute(Request::new(
                r#"mutation { probeModels(baseUrl: "http://127.0.0.1:1/v1") }"#,
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
            Some("\"UPSTREAM\"".to_string())
        );
    }

    /// A server whose headers already satisfy it needs no sign-in, and saying so
    /// is a success: the question was whether one was needed.
    #[tokio::test]
    async fn a_server_that_needs_no_login_says_so() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        // Publishes no OAuth metadata, so a discovery would fail loudly.
        let mcp = Router::new().route("/mcp", post(|| async { axum::http::StatusCode::OK }));
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, mcp,
        )));

        crate::commands::serve::testutil::with_home(|home| async move {
            let paths = crate::commands::serve::mcp::AdminPaths {
                config: home.join("config.toml"),
                store: home.join("mcp-auth.json"),
                grants: home.join("grants.json"),
            };
            std::fs::write(&paths.config, "").expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    let added = schema(true)
                        .execute(Request::new(format!(
                            r#"mutation {{ addMcpServer(name: "hub", url: "{base}/mcp") }}"#
                        )))
                        .await;
                    assert!(added.errors.is_empty(), "{:?}", added.errors);

                    let logged_in = schema(true)
                        .execute(Request::new(r#"mutation { loginMcpServer(name: "hub") }"#))
                        .await;
                    assert!(logged_in.errors.is_empty(), "{:?}", logged_in.errors);
                    let json = serde_json::to_value(&logged_in.data).expect("data serializes");
                    assert_eq!(json["loginMcpServer"], "NOT_REQUIRED");
                })
                .await;
        })
        .await;
    }

    /// A stdio server cannot be signed in to at all, and the refusal says why
    /// rather than reporting a failed handshake.
    #[tokio::test]
    async fn a_stdio_server_cannot_be_signed_in_to() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let paths = crate::commands::serve::mcp::AdminPaths {
                config: home.join("config.toml"),
                store: home.join("mcp-auth.json"),
                grants: home.join("grants.json"),
            };
            std::fs::write(&paths.config, "").expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    schema(true)
                        .execute(Request::new(
                            r#"mutation { addMcpServer(name: "local", command: "/bin/echo") }"#,
                        ))
                        .await;
                    let answer = schema(true)
                        .execute(Request::new(
                            r#"mutation { loginMcpServer(name: "local") }"#,
                        ))
                        .await;
                    let error = answer.errors.first().expect("a refusal");
                    assert!(
                        error.message.contains("HTTP transport"),
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
        })
        .await;
    }

    /// Testing a server that will not start reports the failure as the server's
    /// rather than as this machine's.
    #[tokio::test]
    async fn testing_a_server_that_will_not_start_is_an_upstream_failure() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let paths = crate::commands::serve::mcp::AdminPaths {
                config: home.join("config.toml"),
                store: home.join("mcp-auth.json"),
                grants: home.join("grants.json"),
            };
            std::fs::write(&paths.config, "").expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    schema(true)
                        .execute(Request::new(
                            r#"mutation { addMcpServer(name: "gone",
                                 command: "/definitely/not/a/program") }"#,
                        ))
                        .await;
                    let answer = schema(true)
                        .execute(Request::new(r#"mutation { testMcpServer(name: "gone") }"#))
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
                        Some("\"UPSTREAM\"".to_string())
                    );
                })
                .await;
        })
        .await;
    }

    /// Checking a provider asks the account, so a provider with no grant is an
    /// upstream refusal rather than a green answer.
    #[tokio::test]
    async fn checking_a_provider_with_no_grant_is_refused() {
        crate::config::with_isolated_config_path_async("graphql-check-provider", |_p| async move {
            let answer = schema(true)
                .execute(Request::new(
                    r#"mutation { checkProvider(provider: "codex") }"#,
                ))
                .await;
            let error = answer.errors.first().expect("a refusal");
            assert_eq!(
                error
                    .extensions
                    .as_ref()
                    .and_then(|e| e.get("code"))
                    .map(ToString::to_string),
                Some("\"UPSTREAM\"".to_string()),
                "{}",
                error.message
            );
        })
        .await;
    }
}

/// The refusals the acts that reach the machine give when their own state says
/// no.
///
/// Each is the arm a happy-path test cannot reach: an update already running, a
/// doctor run already going, a sign-in for a provider whose flow will not start.
#[tokio::test]
async fn the_machine_acts_refuse_when_their_own_state_says_no() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        let agents = home.join("agents");
        state.update_jobs = crate::commands::serve::update_job::UpdateJobs::with_env(
            std::sync::Arc::new(move || crate::commands::update::UpdateEnv {
                agents_dir: agents.clone(),
                runner: std::sync::Arc::new(|_argv: &[String]| Ok(())),
                ..crate::commands::update::UpdateEnv::for_planning_offline()
            }),
        );
        // A job already running, so the next request is the conflict arm: two
        // package-manager upgrades of one binary racing is not a state to debug.
        let running = state
            .update_jobs
            .start()
            .expect("nothing else is running")
            .id;
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state.clone())
            .data(super::AdminAccess(true))
            .finish();

        let refused = schema
            .execute(Request::new("mutation { startUpdate { id } }"))
            .await;
        let error = refused.errors.first().expect("a refusal");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"CONFLICT\"".to_string())
        );
        assert!(
            error.message.contains(&running),
            "it names the one that is going: {}",
            error.message
        );
    })
    .await;
}

/// The admin inputs round-trip through their own value form, like every other
/// input object.
#[test]
fn the_admin_inputs_round_trip() {
    use async_graphql::InputType;

    let row = MimeRowInput {
        mime_type: "image/webp".to_string(),
        family: Some("image".to_string()),
        is_text: Some(false),
        extensions: Some(vec!["webp".to_string()]),
        magic: Some("52494646".to_string()),
        stand_in: Some("[a picture]".to_string()),
        check: Some("checks/webp.rhai".to_string()),
        tokens: Some(MimeTokensInput {
            per_byte: None,
            per_pixel: Some(750),
            max: Some(1_600),
            per_second: None,
            per_page: None,
            fixed: None,
        }),
    };
    let Ok(read_back) = MimeRowInput::parse(Some(row.to_value())) else {
        panic!("a mime row reads back from its own value");
    };
    assert_eq!(read_back.mime_type, "image/webp");
    assert_eq!(read_back.is_text, Some(false));
    let tokens = read_back.tokens.expect("the rates come with it");
    assert_eq!(tokens.per_pixel, Some(750));
    assert_eq!(tokens.max, Some(1_600));

    let config = crate::commands::serve::graphql::config_input::ConfigInput {
        default_provider: Some("openai".to_string()),
        provider_order: Some(vec!["openai".to_string()]),
        override_model: async_graphql::MaybeUndefined::Value("gpt-5.6".to_string()),
        fallback_model: async_graphql::MaybeUndefined::Null,
        anthropic_key: async_graphql::MaybeUndefined::Undefined,
        openai_key: async_graphql::MaybeUndefined::Value("sk-o".to_string()),
        google_key: async_graphql::MaybeUndefined::Undefined,
        openrouter_key: async_graphql::MaybeUndefined::Undefined,
        bedrock_key: async_graphql::MaybeUndefined::Undefined,
        xai_key: async_graphql::MaybeUndefined::Undefined,
        meta_key: async_graphql::MaybeUndefined::Undefined,
        bedrock_region: Some("us-east-1".to_string()),
        ollama_base_url: Some("http://127.0.0.1:11434".to_string()),
        ollama_enabled: Some(true),
        codex_enabled: Some(false),
        grok_enabled: Some(true),
        file_uploads: Some(true),
        codex_reasoning_effort: Some("high".to_string()),
        codex_verbosity: Some("low".to_string()),
        codex_replay_reasoning: Some(true),
        gateways: Some(vec![
            crate::commands::serve::graphql::config_input::GatewayInput {
                name: "local".to_string(),
                kind: Some("openai-compatible".to_string()),
                base_url: Some("http://127.0.0.1:1234/v1".to_string()),
                script: None,
                api_key: Some("sk-l".to_string()),
                headers: Some(vec![
                    crate::commands::serve::graphql::config_input::EnvEntryInput {
                        name: "X-Thing".to_string(),
                        value: "1".to_string(),
                    },
                ]),
                models: Some(vec!["llama".to_string()]),
            },
        ]),
        remove_gateways: Some(vec!["old".to_string()]),
    };
    let value = config.to_value();
    let Ok(read_back) =
        crate::commands::serve::graphql::config_input::ConfigInput::parse(Some(value))
    else {
        panic!("a config edit reads back from its own value");
    };
    // A value stays a value and a null stays a clear. An absent field comes back
    // as a null, because the value form has no way to say "not sent": that is a
    // property of the echo rather than of the write, and the write is what the
    // server reads. `a_config_write_sets_clears_and_leaves_alone` is where the
    // third state is held to, over the wire.
    let request = read_back.into_request();
    assert_eq!(request.override_model, Some(Some("gpt-5.6".to_string())));
    assert_eq!(request.fallback_model, Some(None), "null stays a clear");
    assert_eq!(request.remove_gateways, Some(vec!["old".to_string()]));
    let gateway = request
        .gateways
        .as_ref()
        .and_then(|gateways| gateways.first())
        .expect("the gateway");
    assert_eq!(gateway.name, "local");
    assert_eq!(
        gateway
            .headers
            .as_ref()
            .and_then(|headers| headers.get("X-Thing")),
        Some(&"1".to_string())
    );
}

/// Every input object refuses what it cannot read.
///
/// Three shapes, because the reader is generated per type and each shape lands
/// in a different branch of it: a value that is not an object at all, one field
/// of the wrong type, and nothing where a required object belongs. A client
/// sending any of them gets a refusal naming the argument, which is what makes a
/// malformed request debuggable from the answer alone.
#[test]
fn an_input_object_refuses_what_it_cannot_read() {
    use crate::commands::serve::graphql::config_input::{ConfigInput, EnvEntryInput, GatewayInput};
    use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

    /// One object with a single field set to `value`.
    fn one(field: &str, value: Value) -> Option<Value> {
        let mut map = IndexMap::new();
        map.insert(Name::new(field), value);
        Some(Value::Object(map))
    }
    let scalar = || Some(Value::String("nope".to_string()));
    let number = || Value::Number(7.into());

    assert!(EnvEntryInput::parse(scalar()).is_err());
    assert!(EnvEntryInput::parse(None).is_err(), "required, not empty");
    assert!(
        EnvEntryInput::parse(one("name", Value::String("n".to_string()))).is_err(),
        "a field left out"
    );
    assert!(GatewayInput::parse(scalar()).is_err());
    assert!(GatewayInput::parse(None).is_err());
    assert!(
        GatewayInput::parse(one("name", number())).is_err(),
        "a number"
    );
    assert!(ConfigInput::parse(scalar()).is_err());
    assert!(ConfigInput::parse(one("defaultProvider", number())).is_err());
    assert!(MimeRowInput::parse(scalar()).is_err());
    assert!(MimeRowInput::parse(None).is_err());
    assert!(MimeRowInput::parse(one("mimeType", number())).is_err());
    assert!(MimeTokensInput::parse(scalar()).is_err());
    assert!(MimeTokensInput::parse(one("perByte", scalar().expect("a string"))).is_err());

    // And each reads back what it does accept, which is the other half of the
    // same generated reader.
    let Ok(gateway) = GatewayInput::parse(one("name", Value::String("house".to_string()))) else {
        panic!("a gateway needs only its name");
    };
    assert_eq!(gateway.name, "house");
    let Ok(config) =
        ConfigInput::parse(one("defaultProvider", Value::String("openai".to_string())))
    else {
        panic!("one setting is a whole edit");
    };
    assert_eq!(config.default_provider.as_deref(), Some("openai"));
    let Ok(row) = MimeRowInput::parse(one("mimeType", Value::String("image/png".to_string())))
    else {
        panic!("a row needs only its type");
    };
    assert_eq!(row.mime_type, "image/png");
    let Ok(tokens) = MimeTokensInput::parse(one(
        "perByte",
        Value::Number(serde_json::Number::from_f64(0.25).expect("a rate")),
    )) else {
        panic!("one rate is enough");
    };
    assert_eq!(tokens.per_byte, Some(0.25));

    // A field sent as null explicitly, which for a three-state setting means
    // "clear it" and is a different branch of the reader from leaving it out.
    let Ok(cleared) = ConfigInput::parse(one("overrideModel", Value::Null)) else {
        panic!("null is a value a three-state field takes");
    };
    assert!(
        matches!(cleared.override_model, async_graphql::MaybeUndefined::Null),
        "null means clear it, not leave it alone"
    );

    // A nested list of objects, which is its own branch again.
    let mut header = IndexMap::new();
    header.insert(Name::new("name"), Value::String("X-Key".to_string()));
    header.insert(Name::new("value"), Value::String("secret".to_string()));
    let mut gateway = IndexMap::new();
    gateway.insert(Name::new("name"), Value::String("house".to_string()));
    gateway.insert(
        Name::new("headers"),
        Value::List(vec![Value::Object(header)]),
    );
    let Ok(with_headers) = GatewayInput::parse(Some(Value::Object(gateway))) else {
        panic!("a gateway carries its headers");
    };
    assert_eq!(
        with_headers.headers.map(|h| h.len()),
        Some(1),
        "the nested objects come through"
    );

    // A field no input object declares is refused rather than ignored, so a
    // typo in a variable is an answer rather than a setting that silently did
    // nothing.
    // A field no input object declares is ignored rather than refused. Worth
    // pinning: it means a client cannot learn about a typo from the answer, so
    // the schema's own field list is the only place that says what is accepted.
    assert!(ConfigInput::parse(one("nonesuch", Value::Null)).is_ok());

    // An object with nothing in it, which is how a required field goes missing.
    let empty = || Some(Value::Object(IndexMap::new()));
    assert!(
        GatewayInput::parse(empty()).is_err(),
        "a gateway needs a name"
    );
    assert!(MimeRowInput::parse(empty()).is_err(), "a row needs a type");
    assert!(
        ConfigInput::parse(empty()).is_ok(),
        "an empty edit changes nothing"
    );
    assert!(
        MimeTokensInput::parse(empty()).is_ok(),
        "no rate is a rate to keep"
    );
    // A first field that reads and a later one that does not. Each field is read
    // in turn, so a setting that gets the last one wrong has to be refused just
    // as squarely as one that gets the first wrong.
    let two = |first: (&str, Value), second: (&str, Value)| {
        let mut map = IndexMap::new();
        map.insert(Name::new(first.0), first.1);
        map.insert(Name::new(second.0), second.1);
        Some(Value::Object(map))
    };
    assert!(
        GatewayInput::parse(two(
            ("name", Value::String("house".to_string())),
            ("models", number()),
        ))
        .is_err(),
        "a list of models is a list"
    );
    assert!(
        ConfigInput::parse(two(
            ("defaultProvider", Value::String("openai".to_string())),
            ("providerOrder", number()),
        ))
        .is_err(),
        "an order is a list"
    );
    assert!(
        MimeRowInput::parse(two(
            ("mimeType", Value::String("image/png".to_string())),
            ("isText", number()),
        ))
        .is_err(),
        "whether the bytes are text is a yes or a no"
    );
    assert!(
        MimeTokensInput::parse(two(
            (
                "perByte",
                Value::Number(serde_json::Number::from_f64(0.25).expect("a rate"))
            ),
            ("perPixel", Value::String("lots".to_string())),
        ))
        .is_err(),
        "pixels per token is a number"
    );
    // And the last field of each, which is read after every other one.
    assert!(
        MimeRowInput::parse(two(
            ("mimeType", Value::String("image/png".to_string())),
            ("tokens", number()),
        ))
        .is_err(),
        "the rates are a structure"
    );
    assert!(
        MimeTokensInput::parse(two(
            (
                "perByte",
                Value::Number(serde_json::Number::from_f64(0.25).expect("a rate"))
            ),
            ("fixed", Value::String("some".to_string())),
        ))
        .is_err(),
        "a fixed count is a number"
    );
    assert!(
        ConfigInput::parse(two(
            ("defaultProvider", Value::String("openai".to_string())),
            ("removeGateways", number()),
        ))
        .is_err(),
        "the gateways to remove are a list"
    );
}

/// A second live doctor run is refused while one is going.
///
/// Two of them would race two throwaway runs and four billed calls against one
/// config, and a double-clicked button means one check. The refusal is a conflict
/// rather than a failure: the caller's request was fine, the timing was not.
#[tokio::test]
async fn a_second_live_doctor_run_is_a_conflict() {
    let _running = crate::commands::serve::doctor::hold_live_run().await;
    let answer = schema(true)
        .execute(Request::new(
            "mutation { runDoctorLive { checks { name } } }",
        ))
        .await;
    let error = answer.errors.first().expect("a refusal");
    assert_eq!(refusal_code(&answer), "\"CONFLICT\"");
    assert!(
        error.message.contains("already in progress"),
        "{}",
        error.message
    );
}

/// A sign-out whose grant store cannot be written says so.
///
/// The store is a file this server rewrites, and a caller that was told the
/// sign-out worked would go on believing the provider is forgotten while the
/// grant is still on disk.
#[tokio::test]
async fn a_sign_out_that_cannot_write_the_store_is_reported() {
    crate::commands::serve::testutil::with_home(|home| async move {
        // A directory where the grants file belongs: it exists, so the write is
        // attempted, and it cannot be a file.
        let grants = home.join("grants.json");
        std::fs::create_dir_all(&grants).expect("a directory in the way");
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants,
        };
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let answer = schema(true)
                    .execute(Request::new(
                        r#"mutation { providerSignOut(provider: "codex") }"#,
                    ))
                    .await;
                assert_eq!(refusal_code(&answer), "\"INTERNAL\"");
            })
            .await;
    })
    .await;
}

/// Both answers a sign-in question can have.
///
/// Neither is a failure: one says a grant was stored, the other says the server
/// wanted none. A client that read the second as an error would show a red mark
/// against a server that is working.
#[test]
fn both_mcp_login_answers_map_across() {
    use crate::commands::serve::mcp::LoginStatus;
    assert_eq!(
        super::McpLoginStatus::from(LoginStatus::Authenticated),
        super::McpLoginStatus::Authenticated
    );
    assert_eq!(
        super::McpLoginStatus::from(LoginStatus::NotRequired),
        super::McpLoginStatus::NotRequired
    );
}

/// A sign-in that cannot take its loopback port is reported, not left waiting.
///
/// The flow binds the port its client id is registered against before it has a
/// URL to announce, so a port already taken is the failure that happens before
/// any browser is involved. The caller is told; it does not sit waiting for a URL
/// that is never coming.
#[tokio::test]
async fn a_sign_in_that_cannot_take_its_port_is_reported() {
    crate::commands::serve::testutil::with_home(|home| async move {
        // Held for the whole test: this is the port the flow will ask for.
        let held = std::net::TcpListener::bind("127.0.0.1:0").expect("a port to hold");
        let taken = held.local_addr().expect("its number").port();

        let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        state.providers.ports = Some(vec![taken]);
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .data(super::AdminAccess(true))
            .finish();
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: home.join("config.toml"),
            store: home.join("mcp-auth.json"),
            grants: home.join("grants.json"),
        };
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let answer = schema
                    .execute(Request::new(
                        r#"mutation { providerSignIn(provider: "codex") { authorizeUrl } }"#,
                    ))
                    .await;
                assert!(
                    !answer.errors.is_empty(),
                    "a port already taken cannot carry a sign-in"
                );
            })
            .await;
    })
    .await;
}
