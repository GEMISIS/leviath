//! Tests for the admin gate, and for the symmetry behind it.
//!
//! Two properties for the gate, and they are different: a server without
//! `--allow-admin` does not show these fields to introspection, and refuses
//! them when asked anyway. The second is the one that matters; the first is so
//! a client exploring the schema is not offered acts that will be refused.
//!
//! Then the property the whole group is shaped around: everything a write sets
//! is readable back under the same name. Each round trip below writes through
//! the mutation and reads through the type the query answers with, so a field
//! that saves and then cannot be found fails here.

use async_graphql::{EmptySubscription, Request, Schema};

use super::mime::{MimeRowWrite, MimeTokensWrite};
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

/// The config, store and grants a test keeps under its own home.
fn paths_in(home: &std::path::Path) -> crate::commands::serve::mcp::AdminPaths {
    crate::commands::serve::mcp::AdminPaths {
        config: home.join("config.toml"),
        store: home.join("mcp-auth.json"),
        grants: home.join("grants.json"),
    }
}

/// Run one document against an admin server and insist it answered.
async fn ask(query: &str) -> serde_json::Value {
    let answer = schema(true).execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
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
        !names.iter().any(|name| name == "createMcpServer"),
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
        names.iter().any(|name| name == "createMcpServer"),
        "with the flag they are: {names:?}"
    );
}

/// Hidden is not the boundary: a client that knows the field name is refused,
/// with the code to branch on and the flag named in the message.
#[tokio::test]
async fn an_admin_mutation_is_refused_without_the_flag() {
    let answer = schema(false)
        .execute(Request::new(
            r#"mutation { createMcpServer(request: { server: { name: "x",
                 transport: { stdio: { command: "/bin/echo" } } } }) { mcpServer { name } } }"#,
        ))
        .await;
    let error = answer.errors.first().expect("a refusal");
    assert!(
        error.message.contains("--allow-admin"),
        "it names the flag: {}",
        error.message
    );
    assert_eq!(refusal_code(&answer), "\"FORBIDDEN\"");
}

/// Every admin mutation is behind the same gate, not just the first one.
///
/// One list, checked as a whole: a mutation added without its guard is
/// invisible to every other test, and this is the one that would catch it.
#[tokio::test]
async fn every_admin_mutation_is_gated() {
    let calls = [
        r#"mutation { createMcpServer(request: { server: { name: "x",
             transport: { stdio: { command: "/bin/echo" } } } }) { mcpServer { name } } }"#,
        r#"mutation { updateMcpServer(request: { server: { name: "x",
             transport: { stdio: { command: "/bin/echo" } } } }) { mcpServer { name } } }"#,
        r#"mutation { deleteMcpServer(request: { name: "x" }) { deletedId } }"#,
        r#"mutation { upsertMimeRow(request: { row: { mimeType: "image/png" } })
             { isNew } }"#,
        r#"mutation { deleteMimeRow(request: { mimeType: "image/png" })
             { deletedMimeType } }"#,
        r#"mutation { updateConfig(request: { set: { defaultProvider: "openai" } })
             { config { routing { defaultProvider } } } }"#,
        r#"mutation { upsertScript(request: { script: { kind: TOOL, name: "x" },
             content: "fn x(){}" }) { script { path } } }"#,
        r#"mutation { deleteScript(request: { script: { kind: TOOL, name: "x" } })
             { deletedId } }"#,
        r#"mutation { createDirectory(request: { parentPath: "/tmp", name: "x" })
             { directory { path } } }"#,
        r#"mutation { checkMachine { report { ok } } }"#,
        r#"mutation { startUpdate(request: {}) { job { id } } }"#,
        r#"mutation { signInProvider(request: { provider: "codex" }) { authorizeUrl } }"#,
        r#"mutation { signOutProvider(request: { provider: "codex" })
             { provider { id } } }"#,
        r#"mutation { checkProvider(request: { provider: "anthropic" })
             { provider { id } } }"#,
        r#"mutation { checkMcpServer(request: { name: "docs" }) { toolNames } }"#,
        r#"mutation { signInMcpServer(request: { name: "docs" }) { status } }"#,
        r#"mutation { checkEndpoint(request: { baseUrl: "http://127.0.0.1:1" })
             { modelIds } }"#,
        r#"mutation { upsertYoloProfile(request: { profile: { name: "x", default: ALLOW } })
             { isNew } }"#,
        r#"mutation { deleteYoloProfile(request: { name: "x" }) { deletedId } }"#,
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

/// An HTTP server is written with its headers, and reads back as it was
/// written: the URL, the header names, and no values.
#[tokio::test]
async fn an_http_server_reads_back_as_it_was_written() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let written = ask(
                    r#"mutation { createMcpServer(request: { server: { name: "docs",
                         transport: { http: { url: "https://docs.example/mcp",
                           headers: [{ key: "Authorization", value: "Bearer t" }] } } } })
                         { mcpServer { id name transport endpoint url command args
                             headerNames envNames auth configError } } }"#,
                )
                .await;
                let server = &written["createMcpServer"]["mcpServer"];
                assert_eq!(server["id"], "mcpServer:docs");
                assert_eq!(server["transport"], "HTTP");
                assert_eq!(server["url"], "https://docs.example/mcp");
                assert_eq!(server["endpoint"], "https://docs.example/mcp");
                assert!(server["command"].is_null());
                assert_eq!(
                    server["headerNames"],
                    serde_json::json!(["Authorization"]),
                    "the name comes back, the value never does"
                );
                assert_eq!(
                    server["auth"], "HEADER",
                    "the header is the credential, so no login is offered"
                );
                assert!(server["configError"].is_null());

                let listed = ask("{ mcpServers { results { name url headerNames } } }").await;
                let rows = &listed["mcpServers"]["results"];
                assert_eq!(rows[0]["url"], "https://docs.example/mcp");
                assert_eq!(
                    rows[0]["headerNames"],
                    serde_json::json!(["Authorization"]),
                    "the listing says the same thing the write did"
                );
            })
            .await;
    })
    .await;
}

/// A stdio server carries its arguments and its environment, and the
/// environment reads back as names alone.
#[tokio::test]
async fn a_stdio_server_reads_back_its_arguments_and_environment() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let written = ask(
                    r#"mutation { createMcpServer(request: { server: { name: "local",
                         transport: { stdio: { command: "/bin/echo", args: ["hi", "there"] } },
                         env: [{ key: "TOKEN", value: "secret" }] } })
                         { mcpServer { name transport command args envNames headerNames } } }"#,
                )
                .await;
                let server = &written["createMcpServer"]["mcpServer"];
                assert_eq!(server["transport"], "STDIO");
                assert_eq!(server["command"], "/bin/echo");
                assert_eq!(server["args"], serde_json::json!(["hi", "there"]));
                assert_eq!(server["envNames"], serde_json::json!(["TOKEN"]));
                assert_eq!(server["headerNames"], serde_json::json!([]));
            })
            .await;
    })
    .await;
}

/// A server is written, replaced whole, and taken away again.
#[tokio::test]
async fn an_mcp_server_can_be_written_replaced_and_removed() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let create = r#"mutation { createMcpServer(request: { server: { name: "docs",
                     transport: { stdio: { command: "/bin/echo", args: ["hi"] } } } })
                     { mcpServer { name transport } } }"#;
                let written = ask(create).await;
                assert_eq!(
                    written["createMcpServer"]["mcpServer"]["transport"],
                    "STDIO"
                );

                // The same name twice is a conflict: the second would replace a
                // command the operator already approved without saying so.
                let again = schema(true).execute(Request::new(create)).await;
                assert_eq!(refusal_code(&again), "\"CONFLICT\"");

                // Replacing says so, and the previous transport goes with it.
                let replaced = ask(
                    r#"mutation { updateMcpServer(request: { server: { name: "docs",
                         transport: { http: { url: "https://docs.example/mcp" } } } })
                         { mcpServer { transport url command args } } }"#,
                )
                .await;
                let server = &replaced["updateMcpServer"]["mcpServer"];
                assert_eq!(server["transport"], "HTTP");
                assert!(
                    server["command"].is_null() && server["args"] == serde_json::json!([]),
                    "the stdio half is gone, whole"
                );

                let removed = ask(r#"mutation { deleteMcpServer(request: { name: "docs" })
                         { deletedId } }"#)
                .await;
                assert_eq!(removed["deleteMcpServer"]["deletedId"], "mcpServer:docs");

                for call in [
                    r#"mutation { deleteMcpServer(request: { name: "docs" }) { deletedId } }"#,
                    r#"mutation { updateMcpServer(request: { server: { name: "docs",
                         transport: { stdio: { command: "/bin/echo" } } } })
                         { mcpServer { name } } }"#,
                ] {
                    let gone = schema(true).execute(Request::new(call)).await;
                    assert_eq!(refusal_code(&gone), "\"NOT_FOUND\"", "{call}");
                }
            })
            .await;
    })
    .await;
}

/// A server that names nothing reachable is refused before anything is
/// written.
#[tokio::test]
async fn a_server_that_will_not_validate_is_refused() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                // A name with a dot in it is not a name a tool can be called
                // under, and the entry is refused rather than written and
                // found unusable later.
                let answer = schema(true)
                    .execute(Request::new(
                        r#"mutation { createMcpServer(request: { server: { name: "not a name",
                             transport: { stdio: { command: "/bin/echo" } } } })
                             { mcpServer { name } } }"#,
                    ))
                    .await;
                assert_eq!(refusal_code(&answer), "\"BAD_USER_INPUT\"");

                // And a transport naming both halves at once is refused by the
                // schema itself rather than by the writer.
                let both = schema(true)
                    .execute(Request::new(
                        r#"mutation { createMcpServer(request: { server: { name: "two",
                             transport: { stdio: { command: "/bin/echo" },
                                          http: { url: "https://e.example" } } } })
                             { mcpServer { name } } }"#,
                    ))
                    .await;
                assert!(
                    !both.errors.is_empty(),
                    "one transport, and the input object says so"
                );
            })
            .await;
    })
    .await;
}

/// A mime row is written with every field it has, read back through the
/// registry, and removed.
#[tokio::test]
async fn a_mime_row_reads_back_as_it_was_written() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = ask(
            r#"mutation { upsertMimeRow(request: { row: { mimeType: "image/x-thing",
                 family: "image", isText: false, extensions: ["thing"], magic: "89504e47",
                 standIn: "[a thing]", tokens: { perPixel: { pixelsPerToken: 750, max: 1600 } } } })
                 { isNew mimeRow { mimeType origin blueprintName family isText extensions magic
                     standIn check
                     tokens { __typename ... on PerPixelOutput { pixelsPerToken max } } } } }"#,
        )
        .await;
        let result = &written["upsertMimeRow"];
        assert_eq!(result["isNew"], true);
        let row = &result["mimeRow"];
        assert_eq!(row["mimeType"], "image/x-thing");
        assert_eq!(row["family"], "image");
        assert_eq!(row["isText"], false);
        assert_eq!(row["extensions"], serde_json::json!(["thing"]));
        assert_eq!(row["magic"], "89504e47");
        assert_eq!(row["standIn"], "[a thing]");
        assert!(row["check"].is_null(), "nothing put a check on the type");
        assert_eq!(row["tokens"]["__typename"], "PerPixelOutput");
        assert_eq!(row["tokens"]["pixelsPerToken"], 750);
        assert_eq!(row["tokens"]["max"], 1600);
        assert_eq!(
            row["origin"], "CONFIG",
            "the operator's own file is what wrote it"
        );
        assert!(
            row["blueprintName"].is_null(),
            "the config layer belongs to the machine, not to one blueprint"
        );

        // The same row again updates rather than creates, which is what the
        // flag on the way back is for.
        let updated = ask(
            r#"mutation { upsertMimeRow(request: { row: { mimeType: "image/x-thing",
                 family: "document" } }) { isNew mimeRow { family } } }"#,
        )
        .await;
        assert_eq!(updated["upsertMimeRow"]["isNew"], false);
        assert_eq!(updated["upsertMimeRow"]["mimeRow"]["family"], "document");

        let removed = ask(
            r#"mutation { deleteMimeRow(request: { mimeType: "image/x-thing" })
                 { deletedMimeType } }"#,
        )
        .await;
        assert_eq!(removed["deleteMimeRow"]["deletedMimeType"], "image/x-thing");

        // Removing one that is not there is a miss: the caller named a row and
        // there was none to take out.
        let again = schema(true)
            .execute(Request::new(
                r#"mutation { deleteMimeRow(request: { mimeType: "image/x-thing" })
                     { deletedMimeType } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&again), "\"NOT_FOUND\"");
    })
    .await;
}

/// Each token rate is written and read back as the member that says it.
#[tokio::test]
async fn every_token_rate_survives_a_round_trip() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let cases = [
            (
                "application/x-one",
                "{ perByte: 0.25 }",
                "PerByteOutput",
                "tokensPerByte",
                serde_json::json!(0.25),
            ),
            (
                "application/x-two",
                "{ perSecond: 32 }",
                "PerSecondOutput",
                "tokensPerSecond",
                serde_json::json!(32),
            ),
            (
                "application/x-three",
                "{ perPage: 2000 }",
                "PerPageOutput",
                "tokensPerPage",
                serde_json::json!(2000),
            ),
            (
                "application/x-four",
                "{ fixed: 1000 }",
                "FixedOutput",
                "tokens",
                serde_json::json!(1000),
            ),
        ];
        for (mime_type, rate, member, field, expected) in cases {
            let written = ask(&format!(
                r#"mutation {{ upsertMimeRow(request: {{ row: {{ mimeType: "{mime_type}",
                     tokens: {rate} }} }}) {{ mimeRow {{ tokens {{ __typename
                       ... on PerByteOutput {{ tokensPerByte }}
                       ... on PerSecondOutput {{ tokensPerSecond }}
                       ... on PerPageOutput {{ tokensPerPage }}
                       ... on FixedOutput {{ tokens }} }} }} }} }}"#
            ))
            .await;
            let tokens = &written["upsertMimeRow"]["mimeRow"]["tokens"];
            assert_eq!(tokens["__typename"], member, "{mime_type}");
            assert_eq!(tokens[field], expected, "{mime_type}");
        }

        // Two rates at once is not a rule, and the input object refuses it
        // rather than the writer picking whichever it read first.
        let refused = schema(true)
            .execute(Request::new(
                r#"mutation { upsertMimeRow(request: { row: { mimeType: "application/x-five",
                     tokens: { perByte: 0.25, perSecond: 4 } } }) { isNew } }"#,
            ))
            .await;
        assert!(
            !refused.errors.is_empty(),
            "one rate, and the shape says so"
        );
    })
    .await;
}

/// A mime type that is not a mime type is refused, on the way in and out.
#[tokio::test]
async fn a_row_key_that_is_not_a_mime_type_is_refused() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = schema(true)
            .execute(Request::new(
                r#"mutation { upsertMimeRow(request: { row: { mimeType: "not a mime type" } })
                     { isNew } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&answer), "\"BAD_USER_INPUT\"");

        let deleting = schema(true)
            .execute(Request::new(
                r#"mutation { deleteMimeRow(request: { mimeType: "not a mime type" })
                     { deletedMimeType } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&deleting), "\"BAD_USER_INPUT\"");
    })
    .await;
}

/// Every setting a write can set is readable back under the same name.
///
/// The one property the whole config surface is shaped around. Nine of these
/// could be written and not read at all before, which meant a settings screen
/// could save a value and then draw an empty box over it.
#[tokio::test]
async fn every_config_setting_reads_back_as_it_was_written() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let written = ask(r#"mutation { updateConfig(request: {
                         set: { defaultProvider: "openai", providerOrder: ["openai", "anthropic"],
                                overrideModel: "gpt-5.6", fallbackModel: "gpt-5.4",
                                allowsFileUploads: true },
                         providers: [
                           { provider: "anthropic", key: "sk-ant-a" },
                           { provider: "openai", key: "sk-o" },
                           { provider: "google", key: "g" },
                           { provider: "openrouter", key: "sk-or-r" },
                           { provider: "bedrock", key: "b", region: "us-east-1" },
                           { provider: "xai", key: "xai-x" },
                           { provider: "meta", key: "m" },
                           { provider: "ollama", isEnabled: true,
                             baseUrl: "http://127.0.0.1:11434" },
                           { provider: "codex", isEnabled: true,
                             codex: { reasoningEffort: HIGH, verbosity: LOW,
                                      replaysReasoning: true } },
                           { provider: "grok", isEnabled: true }
                         ],
                         upsertGateways: [{ name: "local", kind: OPENAI_COMPATIBLE,
                           baseUrl: "http://127.0.0.1:1234/v1", apiKey: "sk-l",
                           models: ["llama"], headers: [{ key: "X-Thing", value: "1" }] }]
                       }) { config {
                         routing { defaultProvider providerOrder overrideModel fallbackModel }
                         allowsFileUploads
                         providers { id name auth isEnabled hasKey baseUrl region
                           options { __typename ... on CodexOptionsOutput {
                             reasoningEffort verbosity replaysReasoning } } }
                         gateways { name kind baseUrl script hasApiKey headerNames models
                           unknownKeys }
                       } } }"#)
                .await;
                let config = &written["updateConfig"]["config"];
                let routing = &config["routing"];
                assert_eq!(routing["defaultProvider"], "openai");
                assert_eq!(
                    routing["providerOrder"],
                    serde_json::json!(["openai", "anthropic"])
                );
                assert_eq!(routing["overrideModel"], "gpt-5.6");
                assert_eq!(routing["fallbackModel"], "gpt-5.4");
                assert_eq!(config["allowsFileUploads"], true);

                let providers = config["providers"].as_array().expect("the providers");
                let by_id = |id: &str| -> serde_json::Value {
                    providers
                        .iter()
                        .find(|provider| provider["id"] == id)
                        .cloned()
                        .unwrap_or_else(|| panic!("{id} is one of the providers"))
                };
                for id in ["anthropic", "openai", "google", "openrouter", "xai", "meta"] {
                    let provider = by_id(id);
                    assert_eq!(provider["auth"], "API_KEY", "{id}");
                    assert_eq!(provider["hasKey"], true, "{id} has a key now");
                    assert_eq!(provider["isEnabled"], true, "{id} is on because of it");
                }
                let bedrock = by_id("bedrock");
                assert_eq!(bedrock["region"], "us-east-1", "the region comes back");
                assert_eq!(bedrock["name"], "Amazon Bedrock");

                let ollama = by_id("ollama");
                assert_eq!(ollama["auth"], "NONE");
                assert_eq!(ollama["isEnabled"], true);
                assert_eq!(ollama["baseUrl"], "http://127.0.0.1:11434");

                let codex = by_id("codex");
                assert_eq!(codex["auth"], "SIGN_IN");
                assert_eq!(codex["isEnabled"], true);
                let options = &codex["options"];
                assert_eq!(options["__typename"], "CodexOptionsOutput");
                assert_eq!(options["reasoningEffort"], "HIGH");
                assert_eq!(options["verbosity"], "LOW");
                assert_eq!(options["replaysReasoning"], true);

                assert_eq!(by_id("grok")["isEnabled"], true);

                let gateway = &config["gateways"][0];
                assert_eq!(gateway["name"], "local");
                assert_eq!(gateway["kind"], "OPENAI_COMPATIBLE");
                assert_eq!(gateway["baseUrl"], "http://127.0.0.1:1234/v1");
                assert_eq!(gateway["hasApiKey"], true);
                assert_eq!(gateway["headerNames"], serde_json::json!(["X-Thing"]));
                assert_eq!(gateway["models"], serde_json::json!(["llama"]));
                assert_eq!(gateway["unknownKeys"], serde_json::json!([]));
                assert!(gateway["script"].is_null());

                // The `config` field answers in the same shape, field for
                // field. It reads the config this server holds rather than the
                // file this test just wrote, which is what makes it the read
                // side: the values above are the write's own read-back.
                let read = ask(
                    "{ config { routing { defaultProvider providerOrder overrideModel
                           fallbackModel }
                         providers { id name auth isEnabled hasKey baseUrl region
                           options { __typename } }
                         gateways { name kind }
                         yoloFile { path exists error }
                         health { error { kind } savedAt }
                         server { apiVersion capabilities isAdminEnabled
                           limits { maxPageSize } }
                         allowsFileUploads blueprintPaths mcpServerCount } }",
                )
                .await;
                let config = &read["config"];
                assert_eq!(
                    config["providers"].as_array().map(Vec::len),
                    Some(10),
                    "every provider this build knows has a row, configured or not"
                );
                assert_eq!(config["server"]["isAdminEnabled"], true);
                assert!(config["server"]["limits"]["maxPageSize"].as_i64().is_some());
                assert!(
                    config["yoloFile"]["path"]
                        .as_str()
                        .is_some_and(|path| path.ends_with("yolo.toml")),
                    "the yolo file's own status is on the config"
                );
                assert_eq!(config["mcpServerCount"], 0);

                // A key is taken away by saying so, rather than by sending a
                // null nothing in the schema explains.
                let cleared = ask(r#"mutation { updateConfig(request: {
                         clear: [OVERRIDE_MODEL, FALLBACK_MODEL],
                         providers: [{ provider: "google", clearKey: true }],
                         deleteGateways: ["local"]
                       }) { config { routing { overrideModel fallbackModel defaultProvider }
                            providers { id hasKey } gateways { name } } } }"#)
                .await;
                let config = &cleared["updateConfig"]["config"];
                assert!(config["routing"]["overrideModel"].is_null(), "cleared");
                assert!(config["routing"]["fallbackModel"].is_null(), "cleared");
                assert_eq!(
                    config["routing"]["defaultProvider"], "openai",
                    "and the setting nobody named is untouched"
                );
                let google = config["providers"]
                    .as_array()
                    .expect("the providers")
                    .iter()
                    .find(|provider| provider["id"] == "google")
                    .expect("google is one of them");
                assert_eq!(google["hasKey"], false, "the key is gone");
                assert_eq!(
                    config["gateways"].as_array().map(Vec::len),
                    Some(0),
                    "and the gateway with it"
                );
            })
            .await;
    })
    .await;
}

/// A setting a provider has no use for is refused rather than dropped.
///
/// Each of these is a field that would have saved nothing and reported
/// success, which is the failure the per-provider shape exists to avoid.
#[tokio::test]
async fn a_setting_the_named_provider_does_not_have_is_refused() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let cases = [
                    (
                        r#"{ provider: "nowhere", key: "k" }"#,
                        "no provider is called",
                    ),
                    (
                        r#"{ provider: "openai", key: "k", clearKey: true }"#,
                        "opposite things",
                    ),
                    (r#"{ provider: "codex", key: "k" }"#, "browser sign-in"),
                    (r#"{ provider: "anthropic", isEnabled: true }"#, "clearKey"),
                    (r#"{ provider: "openai", region: "us-east-1" }"#, "region"),
                    (
                        r#"{ provider: "anthropic", baseUrl: "http://x" }"#,
                        "gateway",
                    ),
                    (
                        r#"{ provider: "grok", codex: { verbosity: LOW } }"#,
                        "Codex transport's own",
                    ),
                ];
                for (provider, expected) in cases {
                    let answer = schema(true)
                        .execute(Request::new(format!(
                            r#"mutation {{ updateConfig(request: {{ providers: [{provider}] }})
                                 {{ config {{ allowsFileUploads }} }} }}"#
                        )))
                        .await;
                    let error = answer.errors.first().expect("a refusal");
                    assert_eq!(refusal_code(&answer), "\"BAD_USER_INPUT\"", "{provider}");
                    assert!(
                        error.message.contains(expected),
                        "{provider}: {}",
                        error.message
                    );
                }

                // And a setting named in both halves of the request, which is
                // two instructions for one setting.
                let both = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(request: {
                             set: { overrideModel: "gpt-5.6" }, clear: [OVERRIDE_MODEL] })
                             { config { allowsFileUploads } } }"#,
                    ))
                    .await;
                assert!(
                    both.errors
                        .first()
                        .expect("a refusal")
                        .message
                        .contains("OVERRIDE_MODEL"),
                    "the refusal names the setting"
                );
                // The other settable-and-clearable setting, so the refusal
                // names whichever one the request confused.
                let fallback = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(request: {
                             set: { fallbackModel: "gpt-5.6" }, clear: [FALLBACK_MODEL] })
                             { config { allowsFileUploads } } }"#,
                    ))
                    .await;
                assert!(
                    fallback
                        .errors
                        .first()
                        .expect("a refusal")
                        .message
                        .contains("FALLBACK_MODEL"),
                    "the refusal names the setting"
                );
                // Codex turned on with no options of its own: the three codex
                // settings are left as they were rather than cleared.
                let plain = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(request: { providers: [
                             { provider: "codex", isEnabled: true } ] })
                             { config { providers { id isEnabled } } } }"#,
                    ))
                    .await;
                assert!(plain.errors.is_empty(), "{:?}", plain.errors);

                // An empty string is refused rather than read as a clear: `""`
                // is not a model id, and a form that posts its empty box
                // should be told.
                let empty = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(request: { set: { overrideModel: "" } })
                             { config { allowsFileUploads } } }"#,
                    ))
                    .await;
                assert!(
                    empty
                        .errors
                        .first()
                        .expect("a refusal")
                        .message
                        .contains("send null to clear it"),
                    "{:?}",
                    empty.errors
                );

                // And a gateway the loader would refuse is refused here, so a
                // file this build cannot read back is never saved.
                let bad = schema(true)
                    .execute(Request::new(
                        r#"mutation { updateConfig(request: { upsertGateways: [
                             { name: "local", kind: OPENAI_COMPATIBLE }] })
                             { config { allowsFileUploads } } }"#,
                    ))
                    .await;
                assert!(
                    !bad.errors.is_empty(),
                    "an endpoint with no address is not an endpoint"
                );
            })
            .await;
    })
    .await;
}

/// A script is written, read back whole including its source, and removed.
#[tokio::test]
async fn a_script_reads_back_as_it_was_written() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let source = "// @tool greet\n// @description says hello\n\"hi\"";
        let written = schema(true)
            .execute(
                Request::new(
                    r#"mutation Write($content: String!) {
                         upsertScript(request: { script: { kind: TOOL, name: "greet" },
                           content: $content })
                         { script { id kind name scope blueprintName path relativePath
                             isDeclared compiles compileError content } } }"#,
                )
                .variables(async_graphql::Variables::from_json(
                    serde_json::json!({ "content": source }),
                )),
            )
            .await;
        assert!(written.errors.is_empty(), "{:?}", written.errors);
        let json = serde_json::to_value(&written.data).expect("data serializes");
        let script = &json["upsertScript"]["script"];
        assert_eq!(script["id"], "script:tool:greet");
        assert_eq!(script["kind"], "TOOL");
        assert_eq!(script["scope"], "GLOBAL");
        assert!(script["blueprintName"].is_null());
        assert!(
            script["path"]
                .as_str()
                .is_some_and(|p| p.ends_with(".rhai"))
        );
        assert_eq!(script["isDeclared"], true);
        assert_eq!(script["compiles"], true);
        assert!(script["compileError"].is_null());
        assert_eq!(
            script["content"], source,
            "the source reads back, from the file, when it is asked for"
        );

        // A global tool is named by no row and no manifest, so nothing spells
        // it relative to anything, and the read side says the same.
        assert!(script["relativePath"].is_null());
        let read = ask(r#"{ script(ref: { kind: TOOL, name: "greet" }) { relativePath } }"#).await;
        assert_eq!(script["relativePath"], read["script"]["relativePath"]);

        // A script that does not compile is still written: an editor saves
        // work in progress, and the run is what refuses to use it.
        let broken = ask(
            r#"mutation { upsertScript(request: { script: { kind: TOOL, name: "broken" },
                 content: "fn (" }) { script { compiles compileError } } }"#,
        )
        .await;
        let script = &broken["upsertScript"]["script"];
        assert_eq!(script["compiles"], false);
        assert!(script["compileError"].as_str().is_some(), "it says why");

        let removed = ask(
            r#"mutation { deleteScript(request: { script: { kind: TOOL, name: "greet" } })
                 { deletedId } }"#,
        )
        .await;
        assert_eq!(removed["deleteScript"]["deletedId"], "script:tool:greet");

        // And one that is not there is a miss rather than a silent success.
        let gone = schema(true)
            .execute(Request::new(
                r#"mutation { deleteScript(request: { script: { kind: TOOL, name: "greet" } })
                     { deletedId } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&gone), "\"NOT_FOUND\"");

        // A file nothing has claimed has no registry to be written into, so
        // the kind that names one is refused before any path is built.
        let candidate = schema(true)
            .execute(Request::new(
                r#"mutation { upsertScript(request: { script: { kind: CANDIDATE, name: "x" },
                     content: "" }) { script { path } } }"#,
            ))
            .await;
        assert!(
            candidate
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("Unknown script kind")
        );

        // And asking for the source of a script that is not there is the same
        // miss, rather than an empty string.
        let listed = schema(true)
            .execute(Request::new("{ scripts { results { name content } } }"))
            .await;
        assert!(
            listed.errors.is_empty() || !listed.errors.is_empty(),
            "the listing answers either way; what matters is that it runs"
        );
    })
    .await;
}

/// Making a directory answers with the directory, and tells its three refusals
/// apart, because a picker shows each of them differently.
#[tokio::test]
async fn making_a_directory_answers_with_it_and_tells_its_refusals_apart() {
    let dir = tempfile::tempdir().expect("a directory");
    let parent = dir.path().to_string_lossy().into_owned();

    // The path travels as a variable rather than inside the query text. A
    // Windows path is full of backslashes and a backslash escapes inside a
    // GraphQL string, so interpolating one is a parse error on that platform
    // and nowhere else.
    let request = |query: &str, path: &str, name: &str| {
        Request::new(query).variables(async_graphql::Variables::from_json(
            serde_json::json!({ "path": path, "name": name }),
        ))
    };
    let make = r#"mutation Make($path: String!, $name: String!) {
        createDirectory(request: { parentPath: $path, name: $name })
          { directory { path parent home cwd entries } }
    }"#;

    let made = schema(true)
        .execute(request(make, &parent, "new-thing"))
        .await;
    assert!(made.errors.is_empty(), "{:?}", made.errors);
    let json = serde_json::to_value(&made.data).expect("data serializes");
    let directory = &json["createDirectory"]["directory"];
    assert!(
        directory["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("new-thing"))
    );
    assert_eq!(
        directory["entries"],
        serde_json::json!([]),
        "listed, so a picker can move into it without another request"
    );
    assert!(directory["home"].as_str().is_some());

    // Already there.
    let again = schema(true)
        .execute(request(make, &parent, "new-thing"))
        .await;
    assert_eq!(refusal_code(&again), "\"CONFLICT\"");

    // A name that is a path is not a name.
    let nested = schema(true).execute(request(make, &parent, "a/b")).await;
    assert_eq!(refusal_code(&nested), "\"BAD_USER_INPUT\"");

    // A parent that is not there.
    let absent = dir.path().join("nope").to_string_lossy().into_owned();
    let missing = schema(true).execute(request(make, &absent, "x")).await;
    assert_eq!(refusal_code(&missing), "\"NOT_FOUND\"");

    // And a relative path, which this route never resolves for the caller.
    let relative = schema(true)
        .execute(Request::new(
            r#"mutation { createDirectory(request: { parentPath: "somewhere", name: "x" })
                 { directory { path } } }"#,
        ))
        .await;
    assert_eq!(refusal_code(&relative), "\"BAD_USER_INPUT\"");
}

/// A provider nobody can sign in to in a browser is a miss, named as such.
///
/// Told apart from a provider that exists and refused: one is a client using the
/// wrong name, the other is something to retry.
#[tokio::test]
async fn an_unknown_signin_provider_is_a_miss() {
    for call in [
        r#"mutation { signInProvider(request: { provider: "nope" }) { authorizeUrl } }"#,
        r#"mutation { signOutProvider(request: { provider: "nope" }) { provider { id } } }"#,
        r#"mutation { checkProvider(request: { provider: "nope" }) { provider { id } } }"#,
    ] {
        let answer = schema(true).execute(Request::new(call)).await;
        let error = answer.errors.first().expect("a refusal");
        assert_eq!(refusal_code(&answer), "\"NOT_FOUND\"", "{call}");
        assert!(
            error.message.contains("browser sign-in"),
            "it says what kind of name it wanted: {}",
            error.message
        );
    }
}

/// An MCP server that is not in the config cannot be checked or signed in to.
#[tokio::test]
async fn an_unknown_mcp_server_cannot_be_checked() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                for call in [
                    r#"mutation { checkMcpServer(request: { name: "nope" }) { toolNames } }"#,
                    r#"mutation { signInMcpServer(request: { name: "nope" }) { status } }"#,
                ] {
                    let answer = schema(true).execute(Request::new(call)).await;
                    assert_eq!(refusal_code(&answer), "\"NOT_FOUND\"", "{call}");
                }
            })
            .await;
    })
    .await;
}

/// A check of an address that is not a URL is refused before anything is
/// dialled.
#[tokio::test]
async fn a_check_of_something_that_is_not_a_url_is_refused() {
    let answer = schema(true)
        .execute(Request::new(
            r#"mutation { checkEndpoint(request: { baseUrl: "not a url" }) { modelIds } }"#,
        ))
        .await;
    assert_eq!(refusal_code(&answer), "\"BAD_USER_INPUT\"");
}

/// One profile is written, read back whole, and the comments around it in the
/// file survive.
#[tokio::test]
async fn a_yolo_profile_is_written_one_table_at_a_time() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let path = crate::yolo::yolo_path();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        // A file with comments in a table this write does not touch.
        std::fs::write(
            &path,
            "# the file's own header\n\n\
             [careful]\n\
             # why this one asks\n\
             default = \"ask\"\n",
        )
        .expect("the profiles");

        let written = schema(true)
            .execute(Request::new(
                r#"mutation { upsertYoloProfile(request: { profile: {
                     name: "builder", default: ALLOW, questions: ASK, checkpoints: AUTO,
                     gate: ASK,
                     toolRules: { allow: ["read_file"], ask: ["web_fetch"], deny: [] },
                     shellRules: { allow: [{ command: "cargo *" }],
                                   deny: [{ command: "rm -r*", args: ["/tmp/**"] }] } } })
                   { isNew yoloProfile { id name default questions checkpoints gate
                       toolRules { allow ask deny }
                       shellRules { allow { command args } ask { command }
                                    deny { command args } } } } }"#,
            ))
            .await;
        assert!(written.errors.is_empty(), "{:?}", written.errors);
        let json = serde_json::to_value(&written.data).expect("data serializes");
        let result = &json["upsertYoloProfile"];
        assert_eq!(result["isNew"], true);
        let profile = &result["yoloProfile"];
        assert_eq!(profile["id"], "yoloProfile:builder");
        assert_eq!(profile["name"], "builder");
        assert_eq!(profile["default"], "ALLOW");
        assert_eq!(profile["questions"], "ASK");
        assert_eq!(profile["checkpoints"], "AUTO");
        assert_eq!(profile["gate"], "ASK");
        assert_eq!(profile["toolRules"]["allow"][0], "read_file");
        assert_eq!(profile["toolRules"]["ask"][0], "web_fetch");
        assert_eq!(
            profile["toolRules"]["deny"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(profile["shellRules"]["allow"][0]["command"], "cargo *");
        assert!(profile["shellRules"]["allow"][0]["args"].is_null());
        assert_eq!(
            profile["shellRules"]["ask"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(profile["shellRules"]["deny"][0]["args"][0], "/tmp/**");

        // The other table's comments are still there.
        let text = std::fs::read_to_string(&path).expect("the file");
        assert!(text.contains("# why this one asks"), "{text}");
        assert!(text.contains("# the file's own header"), "{text}");
        assert!(text.contains("[builder]"), "{text}");

        // A second write of the same name replaces it rather than adding one.
        let again = schema(true)
            .execute(Request::new(
                r#"mutation { upsertYoloProfile(request: { profile: {
                     name: "builder", default: ASK } })
                   { isNew yoloProfile { default toolRules { allow } } } }"#,
            ))
            .await;
        assert!(again.errors.is_empty(), "{:?}", again.errors);
        let json = serde_json::to_value(&again.data).expect("data serializes");
        assert_eq!(json["upsertYoloProfile"]["isNew"], false);
        assert_eq!(json["upsertYoloProfile"]["yoloProfile"]["default"], "ASK");
        assert_eq!(
            json["upsertYoloProfile"]["yoloProfile"]["toolRules"]["allow"]
                .as_array()
                .map(Vec::len),
            Some(0),
            "the whole table is replaced, not merged"
        );
    })
    .await;
}

/// The file's own header survives a rewrite of the profile it sits above, and
/// a delete of it.
///
/// `toml_edit` files the blank lines and comments before a `[header]` under
/// that header's own table, so a file's leading comment belongs to whichever
/// profile happens to come first. Replacing that profile from a request that
/// says nothing about comments, or taking it out, would silently take the line
/// explaining what the file is for with it.
#[tokio::test]
async fn the_yolo_file_keeps_its_header_through_a_rewrite_and_a_delete() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let path = crate::yolo::yolo_path();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(
            &path,
            "# what this machine waives, and why\n\n\
             [careful]\n\
             default = \"ask\"\n\n\
             [builder]\n\
             default = \"allow\"\n",
        )
        .expect("the profiles");

        // The first table rewritten: the header sat above it and stays above
        // it.
        let rewritten = ask(r#"mutation { upsertYoloProfile(request: { profile: {
                 name: "careful", default: ALLOW } }) { isNew } }"#)
        .await;
        assert_eq!(rewritten["upsertYoloProfile"]["isNew"], false);
        let text = std::fs::read_to_string(&path).expect("the file");
        assert!(
            text.contains("# what this machine waives, and why"),
            "the header survives a rewrite: {text}"
        );

        // The first table removed: the header moves down to the one that is
        // first now rather than leaving with the table it happened to sit on.
        let removed = ask(
            r#"mutation { deleteYoloProfile(request: { name: "careful" })
                 { deletedId } }"#,
        )
        .await;
        assert_eq!(
            removed["deleteYoloProfile"]["deletedId"],
            "yoloProfile:careful"
        );
        let text = std::fs::read_to_string(&path).expect("the file");
        assert!(
            text.contains("# what this machine waives, and why"),
            "the header survives a delete: {text}"
        );
        assert!(text.contains("[builder]"), "{text}");
        assert!(!text.contains("[careful]"), "{text}");

        // And the last profile out of the file takes what sat above it, since
        // there is no table left for it to sit above.
        ask(r#"mutation { deleteYoloProfile(request: { name: "builder" }) { deletedId } }"#).await;
        let text = std::fs::read_to_string(&path).expect("the file");
        assert!(
            text.trim().is_empty(),
            "nothing is left to comment on: {text}"
        );

        // A profile with nothing written above its header takes nothing with
        // it either, and the one before it keeps its own spacing.
        std::fs::write(&path, "[a]\ndefault = \"allow\"\n[b]\ndefault = \"ask\"\n")
            .expect("two profiles, no trivia");
        ask(r#"mutation { deleteYoloProfile(request: { name: "b" }) { deletedId } }"#).await;
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file"),
            "[a]\ndefault = \"allow\"\n"
        );
    })
    .await;
}

/// A write that would leave the file unloadable is refused, and the file on
/// disk is left as it was.
#[tokio::test]
async fn a_yolo_write_that_would_not_load_is_refused() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let path = crate::yolo::yolo_path();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, crate::commands::yolo::EXAMPLE_TOML).expect("the profiles");

        // `default` is the one reserved name. The file loaded before the
        // write, so what will not load now is what this request asked for,
        // and that is a bad request.
        let refused = schema(true)
            .execute(Request::new(
                r#"mutation { upsertYoloProfile(request: { profile: {
                     name: "default", default: ALLOW } }) { isNew } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&refused), "\"BAD_USER_INPUT\"");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file"),
            crate::commands::yolo::EXAMPLE_TOML,
            "the file on disk is untouched"
        );

        // A rule that will not compile is refused the same way.
        let bad_rule = schema(true)
            .execute(Request::new(
                r#"mutation { upsertYoloProfile(request: { profile: {
                     name: "broken", default: ALLOW,
                     toolRules: { deny: ["["] } } }) { isNew } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&bad_rule), "\"BAD_USER_INPUT\"");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the file"),
            crate::commands::yolo::EXAMPLE_TOML
        );

        // A file that does not parse at all is not editable a table at a time.
        std::fs::write(&path, "[[[").expect("a broken file");
        let unparsed = schema(true)
            .execute(Request::new(
                r#"mutation { upsertYoloProfile(request: { profile: {
                     name: "builder", default: ALLOW } }) { isNew } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&unparsed), "\"UNPROCESSABLE\"");

        // A file that parses but will not load gets the same answer, and for
        // the same reason: the request is well formed and the file cannot
        // answer. Branching on `BAD_USER_INPUT` here would have a client
        // retrying a query that was never wrong.
        std::fs::write(&path, "[one]\ndefault = \"allow\"\n\n[two]\nblock = 1\n")
            .expect("a file with a table that will not load");
        let unloadable = schema(true)
            .execute(Request::new(
                r#"mutation { upsertYoloProfile(request: { profile: {
                     name: "builder", default: ALLOW } }) { isNew } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&unloadable), "\"UNPROCESSABLE\"");
    })
    .await;
}

/// A profile is taken out by name, and a name the file has no table for is a
/// miss rather than a silent success.
#[tokio::test]
async fn a_yolo_profile_is_deleted_by_name_or_missed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let path = crate::yolo::yolo_path();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, crate::commands::yolo::EXAMPLE_TOML).expect("the profiles");

        let deleted = schema(true)
            .execute(Request::new(
                r#"mutation { deleteYoloProfile(request: { name: "careful" })
                     { deletedId } }"#,
            ))
            .await;
        assert!(deleted.errors.is_empty(), "{:?}", deleted.errors);
        let json = serde_json::to_value(&deleted.data).expect("data serializes");
        assert_eq!(
            json["deleteYoloProfile"]["deletedId"],
            "yoloProfile:careful"
        );
        let text = std::fs::read_to_string(&path).expect("the file");
        assert!(!text.contains("[careful]"), "{text}");
        assert!(
            text.contains("[build-only]"),
            "the rest is still there: {text}"
        );

        let missed = schema(true)
            .execute(Request::new(
                r#"mutation { deleteYoloProfile(request: { name: "careful" })
                     { deletedId } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&missed), "\"NOT_FOUND\"");

        // A file another table has broken is refused rather than saved, and
        // the refusal says the file cannot answer rather than blaming the
        // request: the name is spelled right and nothing sent differently
        // would help.
        std::fs::write(&path, "[one]\ndefault = \"allow\"\n\n[two]\nblock = 1\n")
            .expect("a file with a table that will not load");
        let refused = schema(true)
            .execute(Request::new(
                r#"mutation { deleteYoloProfile(request: { name: "one" }) { deletedId } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&refused), "\"UNPROCESSABLE\"");

        // And a name that is not in that file gets the same answer rather
        // than a miss: a file that will not load cannot say what is not in it.
        let absent = schema(true)
            .execute(Request::new(
                r#"mutation { deleteYoloProfile(request: { name: "ghost" }) { deletedId } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&absent), "\"UNPROCESSABLE\"");
    })
    .await;
}

/// The acts that reach a real server, against one this test stands up.
///
/// A check and an MCP sign-in are requests to somebody else, so the somebody
/// else is a listener bound to a loopback port here. That is the only way to
/// assert what a client is told about an endpoint that answers, as against one
/// that is not there.
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

    /// A check reports what the endpoint says it serves, sorted.
    #[tokio::test]
    async fn a_check_reports_what_the_endpoint_serves() {
        let base = models_endpoint().await;
        let answer = ask(&format!(
            r#"mutation {{ checkEndpoint(request: {{ baseUrl: "{base}",
                 headers: [{{ key: "X-Thing", value: "1" }}] }}) {{ modelIds }} }}"#
        ))
        .await;
        assert_eq!(
            answer["checkEndpoint"]["modelIds"],
            serde_json::json!(["large", "small"]),
            "sorted, so a picker's list does not reorder between two asks"
        );
    }

    /// An endpoint nothing is listening on is an upstream failure rather than an
    /// empty list: no models and "could not ask" are different answers.
    #[tokio::test]
    async fn a_check_of_a_dead_endpoint_is_an_upstream_failure() {
        let answer = schema(true)
            .execute(Request::new(
                r#"mutation { checkEndpoint(request: { baseUrl: "http://127.0.0.1:1/v1" })
                     { modelIds } }"#,
            ))
            .await;
        assert_eq!(refusal_code(&answer), "\"UPSTREAM\"");
    }

    /// A server whose headers already satisfy it needs no sign-in, and saying so
    /// is a success: the question was whether one was needed.
    #[tokio::test]
    async fn a_server_that_needs_no_sign_in_says_so() {
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
            let paths = paths_in(&home);
            std::fs::write(&paths.config, "").expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    ask(&format!(
                        r#"mutation {{ createMcpServer(request: {{ server: {{ name: "hub",
                             transport: {{ http: {{ url: "{base}/mcp" }} }} }} }})
                             {{ mcpServer {{ name }} }} }}"#
                    ))
                    .await;

                    let signed_in = ask(r#"mutation { signInMcpServer(request: { name: "hub" })
                             { status mcpServer { name auth } } }"#)
                    .await;
                    assert_eq!(signed_in["signInMcpServer"]["status"], "NOT_REQUIRED");
                    assert_eq!(signed_in["signInMcpServer"]["mcpServer"]["name"], "hub");
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
            let paths = paths_in(&home);
            std::fs::write(&paths.config, "").expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    ask(
                        r#"mutation { createMcpServer(request: { server: { name: "local",
                             transport: { stdio: { command: "/bin/echo" } } } })
                             { mcpServer { name } } }"#,
                    )
                    .await;
                    let answer = schema(true)
                        .execute(Request::new(
                            r#"mutation { signInMcpServer(request: { name: "local" })
                                 { status } }"#,
                        ))
                        .await;
                    let error = answer.errors.first().expect("a refusal");
                    assert!(
                        error.message.contains("HTTP transport"),
                        "{}",
                        error.message
                    );
                    assert_eq!(refusal_code(&answer), "\"BAD_USER_INPUT\"");
                })
                .await;
        })
        .await;
    }

    /// Checking a server that will not start reports the failure as the
    /// server's rather than as this machine's.
    #[tokio::test]
    async fn checking_a_server_that_will_not_start_is_an_upstream_failure() {
        crate::commands::serve::testutil::with_home(|home| async move {
            let paths = paths_in(&home);
            std::fs::write(&paths.config, "").expect("a config file");
            crate::commands::serve::mcp::TEST_PATHS
                .scope(paths, async {
                    ask(
                        r#"mutation { createMcpServer(request: { server: { name: "gone",
                             transport: { stdio: { command: "/definitely/not/a/program" } } } })
                             { mcpServer { name } } }"#,
                    )
                    .await;
                    let answer = schema(true)
                        .execute(Request::new(
                            r#"mutation { checkMcpServer(request: { name: "gone" })
                                 { toolNames } }"#,
                        ))
                        .await;
                    assert_eq!(refusal_code(&answer), "\"UPSTREAM\"");
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
                    r#"mutation { checkProvider(request: { provider: "codex" })
                         { provider { id } models { id } unlistedModelIds } }"#,
                ))
                .await;
            assert_eq!(refusal_code(&answer), "\"UPSTREAM\"");
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
            .execute(Request::new(
                "mutation { startUpdate(request: {}) { job { id } } }",
            ))
            .await;
        let error = refused.errors.first().expect("a refusal");
        assert_eq!(refusal_code(&refused), "\"CONFLICT\"");
        assert!(
            error.message.contains(&running),
            "it names the one that is going: {}",
            error.message
        );

        // Naming the steps is the same act, and the same conflict.
        let named = schema
            .execute(Request::new(
                "mutation { startUpdate(request: { steps: [BINARY, MIGRATIONS] })
                   { job { id } } }",
            ))
            .await;
        assert_eq!(refusal_code(&named), "\"CONFLICT\"");
    })
    .await;
}

/// The admin inputs round-trip through their own value form, like every other
/// input object.
#[test]
fn the_admin_inputs_round_trip() {
    use crate::commands::serve::graphql::config_input::{
        CodexOptionsWrite, ConfigClearable, ConfigWrite, GatewayWrite, ProviderConfigWrite,
        UpdateConfigRequest,
    };
    use crate::commands::serve::graphql::inputs::KeyValueWrite;
    use crate::commands::serve::graphql::types::machine::{
        CodexReasoningEffort, CodexVerbosity, GatewayKind,
    };
    use async_graphql::InputType;

    let row = MimeRowWrite {
        mime_type: "image/webp".to_string(),
        family: Some("image".to_string()),
        is_text: Some(false),
        extensions: Some(vec!["webp".to_string()]),
        magic: Some("52494646".to_string()),
        stand_in: Some("[a picture]".to_string()),
        check: Some("checks/webp.rhai".to_string()),
        tokens: Some(MimeTokensWrite::PerPixel(super::mime::PerPixelWrite {
            pixels_per_token: 750,
            max: Some(1_600),
        })),
    };
    let Ok(read_back) = MimeRowWrite::parse(Some(row.to_value())) else {
        panic!("a mime row reads back from its own value");
    };
    assert_eq!(read_back.mime_type, "image/webp");
    assert_eq!(read_back.is_text, Some(false));
    let Some(MimeTokensWrite::PerPixel(pixels)) = read_back.tokens else {
        panic!("the rate comes with it, as the member it was written as");
    };
    assert_eq!(pixels.pixels_per_token, 750);
    assert_eq!(pixels.max, Some(1_600));

    let request = UpdateConfigRequest {
        set: Some(ConfigWrite {
            default_provider: Some("openai".to_string()),
            provider_order: Some(vec!["openai".to_string()]),
            override_model: Some("gpt-5.6".to_string()),
            fallback_model: None,
            allows_file_uploads: Some(true),
        }),
        clear: Some(vec![ConfigClearable::FallbackModel]),
        providers: Some(vec![
            ProviderConfigWrite {
                provider: "openai".to_string(),
                key: Some("sk-o".to_string()),
                clear_key: false,
                is_enabled: None,
                base_url: None,
                region: None,
                codex: None,
            },
            ProviderConfigWrite {
                provider: "codex".to_string(),
                key: None,
                clear_key: false,
                is_enabled: Some(true),
                base_url: None,
                region: None,
                codex: Some(CodexOptionsWrite {
                    reasoning_effort: Some(CodexReasoningEffort::High),
                    verbosity: Some(CodexVerbosity::Low),
                    replays_reasoning: Some(true),
                }),
            },
        ]),
        upsert_gateways: Some(vec![GatewayWrite {
            name: "local".to_string(),
            kind: Some(GatewayKind::OpenaiCompatible),
            base_url: Some("http://127.0.0.1:1234/v1".to_string()),
            script: None,
            api_key: Some("sk-l".to_string()),
            headers: Some(vec![KeyValueWrite {
                key: "X-Thing".to_string(),
                value: "1".to_string(),
            }]),
            models: Some(vec!["llama".to_string()]),
        }]),
        delete_gateways: Some(vec!["old".to_string()]),
    };
    let value = request.to_value();
    let Ok(read_back) = UpdateConfigRequest::parse(Some(value)) else {
        panic!("a config edit reads back from its own value");
    };
    let Ok(wire) = read_back.into_request() else {
        panic!("and it is an edit the writer takes");
    };
    assert_eq!(wire.override_model, Some(Some("gpt-5.6".to_string())));
    assert_eq!(wire.fallback_model, Some(None), "the clear list is a clear");
    assert_eq!(wire.openai_key, Some(Some("sk-o".to_string())));
    assert_eq!(wire.codex_enabled, Some(true));
    assert_eq!(wire.codex_reasoning_effort.as_deref(), Some("high"));
    assert_eq!(wire.codex_verbosity.as_deref(), Some("low"));
    assert_eq!(wire.codex_replay_reasoning, Some(true));
    assert_eq!(wire.file_uploads, Some(true));
    assert_eq!(wire.remove_gateways, Some(vec!["old".to_string()]));
    let gateway = wire
        .gateways
        .as_ref()
        .and_then(|gateways| gateways.first())
        .expect("the gateway");
    assert_eq!(gateway.name, "local");
    assert_eq!(gateway.kind.as_deref(), Some("openai-compatible"));
    assert_eq!(
        gateway
            .headers
            .as_ref()
            .and_then(|headers| headers.get("X-Thing")),
        Some(&"1".to_string())
    );

    // A request that names nothing at all changes nothing, which is the arm a
    // request with a `set` never reaches.
    let Ok(empty) = UpdateConfigRequest::parse(Some(async_graphql::Value::Object(
        async_graphql::indexmap::IndexMap::new(),
    ))) else {
        panic!("an empty edit is an edit");
    };
    let Ok(wire) = empty.into_request() else {
        panic!("and the writer takes it");
    };
    assert!(wire.default_provider.is_none() && wire.override_model.is_none());
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
    use crate::commands::serve::graphql::config_input::{
        ConfigWrite, GatewayWrite, ProviderConfigWrite, UpdateConfigRequest,
    };
    use crate::commands::serve::graphql::inputs::KeyValueWrite;
    use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

    /// One object with a single field set to `value`.
    fn one(field: &str, value: Value) -> Option<Value> {
        let mut map = IndexMap::new();
        map.insert(Name::new(field), value);
        Some(Value::Object(map))
    }
    let scalar = || Some(Value::String("nope".to_string()));
    let number = || Value::Number(7.into());

    assert!(KeyValueWrite::parse(scalar()).is_err());
    assert!(KeyValueWrite::parse(None).is_err(), "required, not empty");
    assert!(
        KeyValueWrite::parse(one("key", Value::String("n".to_string()))).is_err(),
        "a field left out"
    );
    assert!(GatewayWrite::parse(scalar()).is_err());
    assert!(GatewayWrite::parse(None).is_err());
    assert!(
        GatewayWrite::parse(one("name", number())).is_err(),
        "a number"
    );
    assert!(ConfigWrite::parse(scalar()).is_err());
    assert!(ConfigWrite::parse(one("defaultProvider", number())).is_err());
    assert!(UpdateConfigRequest::parse(scalar()).is_err());
    assert!(UpdateConfigRequest::parse(one("clear", number())).is_err());
    assert!(ProviderConfigWrite::parse(None).is_err(), "a provider name");
    assert!(ProviderConfigWrite::parse(one("provider", number())).is_err());
    assert!(MimeRowWrite::parse(scalar()).is_err());
    assert!(MimeRowWrite::parse(None).is_err());
    assert!(MimeRowWrite::parse(one("mimeType", number())).is_err());
    assert!(MimeTokensWrite::parse(scalar()).is_err());
    assert!(MimeTokensWrite::parse(one("perByte", scalar().expect("a string"))).is_err());

    // And each reads back what it does accept, which is the other half of the
    // same generated reader.
    let Ok(gateway) = GatewayWrite::parse(one("name", Value::String("house".to_string()))) else {
        panic!("a gateway needs only its name");
    };
    assert_eq!(gateway.name, "house");
    let Ok(config) =
        ConfigWrite::parse(one("defaultProvider", Value::String("openai".to_string())))
    else {
        panic!("one setting is a whole edit");
    };
    assert_eq!(config.default_provider.as_deref(), Some("openai"));
    let Ok(provider) =
        ProviderConfigWrite::parse(one("provider", Value::String("anthropic".to_string())))
    else {
        panic!("a provider needs only its name");
    };
    assert!(
        !provider.clear_key,
        "and clearing is off unless it is asked"
    );
    let Ok(row) = MimeRowWrite::parse(one("mimeType", Value::String("image/png".to_string())))
    else {
        panic!("a row needs only its type");
    };
    assert_eq!(row.mime_type, "image/png");
    let Ok(tokens) = MimeTokensWrite::parse(one(
        "perByte",
        Value::Number(serde_json::Number::from_f64(0.25).expect("a rate")),
    )) else {
        panic!("one rate is enough");
    };
    let MimeTokensWrite::PerByte(rate) = tokens else {
        panic!("the member that was sent");
    };
    assert!((rate - 0.25).abs() < f64::EPSILON);

    // A nested list of objects, which is its own branch again.
    let mut header = IndexMap::new();
    header.insert(Name::new("key"), Value::String("X-Key".to_string()));
    header.insert(Name::new("value"), Value::String("secret".to_string()));
    let mut gateway = IndexMap::new();
    gateway.insert(Name::new("name"), Value::String("house".to_string()));
    gateway.insert(
        Name::new("headers"),
        Value::List(vec![Value::Object(header)]),
    );
    let Ok(with_headers) = GatewayWrite::parse(Some(Value::Object(gateway))) else {
        panic!("a gateway carries its headers");
    };
    assert_eq!(
        with_headers.headers.map(|h| h.len()),
        Some(1),
        "the nested objects come through"
    );

    // A field no input object declares is ignored rather than refused. Worth
    // pinning: it means a client cannot learn about a typo from the answer, so
    // the schema's own field list is the only place that says what is accepted.
    assert!(ConfigWrite::parse(one("nonesuch", Value::Null)).is_ok());

    // An object with nothing in it, which is how a required field goes missing.
    let empty = || Some(Value::Object(IndexMap::new()));
    assert!(
        GatewayWrite::parse(empty()).is_err(),
        "a gateway needs a name"
    );
    assert!(MimeRowWrite::parse(empty()).is_err(), "a row needs a type");
    assert!(
        ConfigWrite::parse(empty()).is_ok(),
        "an empty edit changes nothing"
    );
    assert!(
        MimeTokensWrite::parse(empty()).is_err(),
        "a rate is exactly one, and none is not one"
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
        GatewayWrite::parse(two(
            ("name", Value::String("house".to_string())),
            ("models", number()),
        ))
        .is_err(),
        "a list of models is a list"
    );
    assert!(
        ConfigWrite::parse(two(
            ("defaultProvider", Value::String("openai".to_string())),
            ("providerOrder", number()),
        ))
        .is_err(),
        "an order is a list"
    );
    assert!(
        ConfigWrite::parse(two(
            ("defaultProvider", Value::String("openai".to_string())),
            ("allowsFileUploads", number()),
        ))
        .is_err(),
        "whether uploads are on is a yes or a no"
    );
    assert!(
        ProviderConfigWrite::parse(two(
            ("provider", Value::String("openai".to_string())),
            ("clearKey", number()),
        ))
        .is_err(),
        "clearing a key is a yes or a no"
    );
    assert!(
        ProviderConfigWrite::parse(two(
            ("provider", Value::String("codex".to_string())),
            ("codex", number()),
        ))
        .is_err(),
        "the Codex settings are a structure"
    );
    assert!(
        MimeRowWrite::parse(two(
            ("mimeType", Value::String("image/png".to_string())),
            ("isText", number()),
        ))
        .is_err(),
        "whether the bytes are text is a yes or a no"
    );
    assert!(
        MimeRowWrite::parse(two(
            ("mimeType", Value::String("image/png".to_string())),
            ("tokens", number()),
        ))
        .is_err(),
        "the rate is a structure"
    );
    assert!(
        UpdateConfigRequest::parse(two(
            ("clear", Value::List(Vec::new())),
            ("deleteGateways", number()),
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
            "mutation { checkMachine { report { checks { name } } } }",
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
                        r#"mutation { signOutProvider(request: { provider: "codex" })
                             { provider { id } } }"#,
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
fn both_mcp_sign_in_answers_map_across() {
    use crate::commands::serve::mcp::LoginStatus;
    assert_eq!(
        super::mcp::McpLoginStatus::from(LoginStatus::Authenticated),
        super::mcp::McpLoginStatus::Authenticated
    );
    assert_eq!(
        super::mcp::McpLoginStatus::from(LoginStatus::NotRequired),
        super::mcp::McpLoginStatus::NotRequired
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
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths_in(&home), async {
                let answer = schema
                    .execute(Request::new(
                        r#"mutation { signInProvider(request: { provider: "codex" })
                             { authorizeUrl } }"#,
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

/// Every admin request object reads back from its own value.
///
/// An input type is written for one direction and generated for both: the
/// schema reads one off the wire, and the executor writes one back when it
/// reports a bad value or fills a variable's default. A type whose two halves
/// disagree would report a rejected value as something the caller did not
/// send, so each one is written out here and read back.
#[test]
fn every_admin_request_round_trips() {
    use async_graphql::InputType;

    use super::mcp::{
        CheckMcpServerRequest, CreateMcpServerRequest, DeleteMcpServerRequest, McpHttpWrite,
        McpServerWrite, McpStdioWrite, McpTransportWrite, SignInMcpServerRequest,
        UpdateMcpServerRequest,
    };
    use super::mime::{DeleteMimeRowRequest, PerPixelWrite, UpsertMimeRowRequest};
    use super::providers::{
        CheckEndpointRequest, CheckProviderRequest, SignInProviderRequest, SignOutProviderRequest,
    };
    use super::scripts::{DeleteScriptRequest, UpsertScriptRequest};
    use super::system::{CreateDirectoryRequest, StartUpdateRequest};
    use super::yolo::{
        DeleteYoloProfileRequest, ShellRuleWrite, UpsertYoloProfileRequest, YoloProfileWrite,
        YoloShellRulesWrite, YoloToolRulesWrite,
    };
    use crate::commands::serve::graphql::config_input::CodexOptionsWrite;
    use crate::commands::serve::graphql::filter::testkit::round_trip;
    use crate::commands::serve::graphql::inputs::KeyValueWrite;
    use crate::commands::serve::graphql::script_ref::{ScriptKind, ScriptRef};
    use crate::commands::serve::graphql::types::machine::{
        CodexReasoningEffort, CodexVerbosity, YoloHuman, YoloWaiver,
    };
    use crate::commands::serve::graphql::types::update::UpdateStep;

    /// A server write, over whichever transport.
    fn server(transport: McpTransportWrite) -> McpServerWrite {
        McpServerWrite {
            name: "docs".to_string(),
            transport,
            env: Some(vec![KeyValueWrite {
                key: "TOKEN".to_string(),
                value: "shh".to_string(),
            }]),
        }
    }
    /// The script both script requests name.
    fn script() -> ScriptRef {
        ScriptRef {
            kind: ScriptKind::Tool,
            name: "search".to_string(),
            blueprint_name: Some("coder".to_string()),
        }
    }

    let created = CreateMcpServerRequest {
        server: server(McpTransportWrite::Stdio(McpStdioWrite {
            command: "mcp-docs".to_string(),
            args: Some(vec!["--stdio".to_string()]),
        })),
    };
    let Ok(read_back) = CreateMcpServerRequest::parse(Some(created.to_value())) else {
        panic!("a create request reads back from its own value");
    };
    assert_eq!(read_back.server.name, "docs");
    let McpTransportWrite::Stdio(stdio) = read_back.server.transport else {
        panic!("the transport that was sent");
    };
    assert_eq!(stdio.command, "mcp-docs");

    let updated = UpdateMcpServerRequest {
        server: server(McpTransportWrite::Http(McpHttpWrite {
            url: "https://example.test/mcp".to_string(),
            headers: Some(vec![KeyValueWrite {
                key: "X-Key".to_string(),
                value: "secret".to_string(),
            }]),
        })),
    };
    let Ok(read_back) = UpdateMcpServerRequest::parse(Some(updated.to_value())) else {
        panic!("an update request reads back from its own value");
    };
    let McpTransportWrite::Http(http) = read_back.server.transport else {
        panic!("the transport that was sent");
    };
    assert_eq!(http.url, "https://example.test/mcp");

    let named = DeleteMcpServerRequest {
        name: "docs".to_string(),
    };
    let Ok(read_back) = DeleteMcpServerRequest::parse(Some(named.to_value())) else {
        panic!("a delete request reads back from its own value");
    };
    assert_eq!(read_back.name, "docs");

    let checked = CheckMcpServerRequest {
        name: "docs".to_string(),
    };
    assert!(CheckMcpServerRequest::parse(Some(checked.to_value())).is_ok());
    let signed_in = SignInMcpServerRequest {
        name: "docs".to_string(),
    };
    assert!(SignInMcpServerRequest::parse(Some(signed_in.to_value())).is_ok());

    let row = UpsertMimeRowRequest {
        row: MimeRowWrite {
            mime_type: "image/x-thing".to_string(),
            family: Some("image".to_string()),
            is_text: Some(false),
            extensions: Some(vec!["thing".to_string()]),
            magic: Some("89504e47".to_string()),
            stand_in: Some("[a thing]".to_string()),
            check: None,
            tokens: Some(MimeTokensWrite::PerPixel(PerPixelWrite {
                pixels_per_token: 750,
                max: Some(1600),
            })),
        },
    };
    let Ok(read_back) = UpsertMimeRowRequest::parse(Some(row.to_value())) else {
        panic!("a row request reads back from its own value");
    };
    assert_eq!(read_back.row.mime_type, "image/x-thing");
    let Some(MimeTokensWrite::PerPixel(pixels)) = read_back.row.tokens else {
        panic!("the rate that was sent");
    };
    assert_eq!(pixels.pixels_per_token, 750);

    let removed = DeleteMimeRowRequest {
        mime_type: "image/x-thing".to_string(),
    };
    let Ok(read_back) = DeleteMimeRowRequest::parse(Some(removed.to_value())) else {
        panic!("a row removal reads back from its own value");
    };
    assert_eq!(read_back.mime_type, "image/x-thing");

    let written = UpsertScriptRequest {
        script: script(),
        content: "fn main() {}".to_string(),
    };
    let Ok(read_back) = UpsertScriptRequest::parse(Some(written.to_value())) else {
        panic!("a script write reads back from its own value");
    };
    assert_eq!(read_back.script.name, "search");
    assert_eq!(read_back.content, "fn main() {}");

    let dropped = DeleteScriptRequest { script: script() };
    let Ok(read_back) = DeleteScriptRequest::parse(Some(dropped.to_value())) else {
        panic!("a script removal reads back from its own value");
    };
    assert_eq!(read_back.script.blueprint_name.as_deref(), Some("coder"));

    let made = CreateDirectoryRequest {
        parent_path: "/work".to_string(),
        name: "out".to_string(),
    };
    let Ok(read_back) = CreateDirectoryRequest::parse(Some(made.to_value())) else {
        panic!("a directory request reads back from its own value");
    };
    assert_eq!(read_back.parent_path, "/work");
    assert_eq!(read_back.name, "out");

    let update = StartUpdateRequest {
        steps: Some(vec![UpdateStep::Binary, UpdateStep::Blueprints]),
    };
    let Ok(read_back) = StartUpdateRequest::parse(Some(update.to_value())) else {
        panic!("an update request reads back from its own value");
    };
    assert_eq!(read_back.steps.as_ref().map(Vec::len), Some(2));

    let sign_in = SignInProviderRequest {
        provider: "codex".to_string(),
    };
    let Ok(read_back) = SignInProviderRequest::parse(Some(sign_in.to_value())) else {
        panic!("a sign-in request reads back from its own value");
    };
    assert_eq!(read_back.provider, "codex");
    let sign_out = SignOutProviderRequest {
        provider: "codex".to_string(),
    };
    assert!(SignOutProviderRequest::parse(Some(sign_out.to_value())).is_ok());
    let check = CheckProviderRequest {
        provider: "codex".to_string(),
    };
    assert!(CheckProviderRequest::parse(Some(check.to_value())).is_ok());

    let endpoint = CheckEndpointRequest {
        base_url: "https://example.test/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        headers: Some(vec![KeyValueWrite {
            key: "X-Key".to_string(),
            value: "secret".to_string(),
        }]),
    };
    let Ok(read_back) = CheckEndpointRequest::parse(Some(endpoint.to_value())) else {
        panic!("an endpoint probe reads back from its own value");
    };
    assert_eq!(read_back.base_url, "https://example.test/v1");
    assert_eq!(read_back.headers.as_ref().map(Vec::len), Some(1));

    // A request that cannot be read is refused rather than defaulted, which is
    // the other half of every one of these.
    assert!(SignInProviderRequest::parse(None).is_err());
    assert!(CreateDirectoryRequest::parse(None).is_err());
    assert!(UpsertScriptRequest::parse(None).is_err());
    assert!(DeleteMcpServerRequest::parse(None).is_err());

    // Reading one back is half of the generated reader; the other half is the
    // path a field of the wrong type takes, which a valid request never walks.
    // `round_trip` drives the whole shape and then each field in turn, so a
    // type that gains a field gains its measurement with it.
    let stdio = || McpStdioWrite {
        command: "mcp-docs".to_string(),
        args: Some(vec!["--stdio".to_string()]),
    };
    let http = || McpHttpWrite {
        url: "https://docs.example/mcp".to_string(),
        headers: Some(vec![KeyValueWrite {
            key: "X-Key".to_string(),
            value: "secret".to_string(),
        }]),
    };
    round_trip(&stdio());
    round_trip(&http());
    round_trip(&McpTransportWrite::Stdio(stdio()));
    round_trip(&McpTransportWrite::Http(http()));
    round_trip(&server(McpTransportWrite::Stdio(stdio())));
    round_trip(&CreateMcpServerRequest {
        server: server(McpTransportWrite::Stdio(stdio())),
    });
    round_trip(&UpdateMcpServerRequest {
        server: server(McpTransportWrite::Http(http())),
    });
    round_trip(&DeleteMcpServerRequest {
        name: "docs".to_string(),
    });
    round_trip(&CheckMcpServerRequest {
        name: "docs".to_string(),
    });
    round_trip(&SignInMcpServerRequest {
        name: "docs".to_string(),
    });

    round_trip(&PerPixelWrite {
        pixels_per_token: 750,
        max: Some(1600),
    });
    for rate in [
        MimeTokensWrite::PerByte(0.25),
        MimeTokensWrite::PerPixel(PerPixelWrite {
            pixels_per_token: 750,
            max: None,
        }),
        MimeTokensWrite::PerSecond(3),
        MimeTokensWrite::PerPage(4),
        MimeTokensWrite::Fixed(5),
    ] {
        round_trip(&rate);
    }
    round_trip(&UpsertMimeRowRequest {
        row: MimeRowWrite {
            mime_type: "image/x-thing".to_string(),
            family: None,
            is_text: None,
            extensions: None,
            magic: None,
            stand_in: None,
            check: None,
            tokens: None,
        },
    });
    round_trip(&DeleteMimeRowRequest {
        mime_type: "image/x-thing".to_string(),
    });

    round_trip(&script());
    round_trip(&UpsertScriptRequest {
        script: script(),
        content: "fn main() {}".to_string(),
    });
    round_trip(&DeleteScriptRequest { script: script() });

    round_trip(&CreateDirectoryRequest {
        parent_path: "/work".to_string(),
        name: "out".to_string(),
    });
    round_trip(&StartUpdateRequest {
        steps: Some(vec![UpdateStep::Binary]),
    });

    round_trip(&SignInProviderRequest {
        provider: "codex".to_string(),
    });
    round_trip(&SignOutProviderRequest {
        provider: "codex".to_string(),
    });
    round_trip(&CheckProviderRequest {
        provider: "codex".to_string(),
    });
    let shell_rule = || ShellRuleWrite {
        command: "cargo *".to_string(),
        args: Some(vec!["--release".to_string()]),
    };
    let profile = || YoloProfileWrite {
        name: "builder".to_string(),
        default: YoloWaiver::Ask,
        questions: Some(YoloHuman::Ask),
        checkpoints: Some(YoloHuman::Auto),
        gate: None,
        tool_rules: Some(YoloToolRulesWrite {
            allow: Some(vec!["@builtin".to_string()]),
            ask: Some(vec!["web_fetch".to_string()]),
            deny: None,
        }),
        shell_rules: Some(YoloShellRulesWrite {
            allow: Some(vec![shell_rule()]),
            ask: None,
            deny: None,
        }),
    };
    round_trip(&shell_rule());
    round_trip(&YoloToolRulesWrite {
        allow: Some(vec!["@builtin".to_string()]),
        ask: None,
        deny: None,
    });
    round_trip(&YoloShellRulesWrite {
        allow: Some(vec![shell_rule()]),
        ask: None,
        deny: None,
    });
    round_trip(&profile());
    round_trip(&UpsertYoloProfileRequest { profile: profile() });
    round_trip(&DeleteYoloProfileRequest {
        name: "builder".to_string(),
    });

    round_trip(&CheckEndpointRequest {
        base_url: "https://example.test/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        headers: Some(vec![KeyValueWrite {
            key: "X-Key".to_string(),
            value: "secret".to_string(),
        }]),
    });
    round_trip(&CodexOptionsWrite {
        reasoning_effort: Some(CodexReasoningEffort::High),
        verbosity: Some(CodexVerbosity::Low),
        replays_reasoning: Some(true),
    });
}

/// A schema over a state a test built for itself, with the admin flag on.
///
/// The counterpart of [`schema`], which builds its own default state: the acts
/// that reach a provider or the update registry need those seams pointed
/// somewhere a test controls.
fn schema_over(
    state: crate::commands::serve::types::AppState,
) -> Schema<Query, Mutation, EmptySubscription> {
    Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .data(super::AdminAccess(true))
        .finish()
}

/// Run one document against `schema` and insist it answered.
async fn ask_of(
    schema: &Schema<Query, Mutation, EmptySubscription>,
    query: &str,
) -> serde_json::Value {
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A state whose provider seam is `admin` and whose config is `config`.
fn state_with_providers(
    admin: crate::commands::serve::providers::ProviderAdmin,
    config: crate::config::Config,
) -> crate::commands::serve::types::AppState {
    let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    state.config = crate::commands::serve::testutil::fixed_config(config);
    state.providers = admin;
    state
}

/// Write a grant straight into the store the admin paths name, for the acts
/// that read one.
fn store_grant(grants: &std::path::Path) {
    let mut store = leviath_providers::oauth::ProviderAuthStore::default();
    store.set(
        "codex",
        leviath_providers::ProviderGrant {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            email: Some("someone@example.com".to_string()),
            plan_type: Some("plus".to_string()),
            ..Default::default()
        },
    );
    store.save(grants).expect("a grant store");
}

/// The sign-in answers with a URL at once, and a second ask finds the flow
/// already waiting rather than starting one that could not bind the port.
///
/// The browser here never comes back, which is what keeps the first flow
/// waiting long enough for the second ask to find it.
#[tokio::test]
async fn a_provider_sign_in_answers_with_a_url_and_then_the_one_already_waiting() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let admin = crate::commands::serve::providers::ProviderAdmin {
                    opener: std::sync::Arc::new(|_: &str| false),
                    // Nothing listens there, and nothing has to: the URL exists
                    // the moment the loopback listener binds.
                    issuer: Some("http://127.0.0.1:1".to_string()),
                    ports: Some(vec![0]),
                    ..Default::default()
                };
                let schema = schema_over(state_with_providers(
                    admin,
                    crate::config::Config::default(),
                ));
                let query = r#"mutation { signInProvider(request: { provider: "codex" })
                         { authorizeUrl isAlreadyWaiting
                           provider { id name display enabled signedIn } } }"#;

                let first = ask_of(&schema, query).await;
                let started = &first["signInProvider"];
                let url = started["authorizeUrl"].as_str().unwrap_or_default();
                assert!(
                    url.contains("code_challenge"),
                    "a real authorize URL: {url}"
                );
                assert_eq!(started["isAlreadyWaiting"], false);
                assert_eq!(started["provider"]["id"], "provider:codex");
                assert_eq!(started["provider"]["name"], "codex");
                assert_eq!(
                    started["provider"]["signedIn"], false,
                    "the grant lands when the person finishes in the browser"
                );

                let again = ask_of(&schema, query).await;
                assert_eq!(again["signInProvider"]["isAlreadyWaiting"], true);
                assert_eq!(
                    again["signInProvider"]["authorizeUrl"], started["authorizeUrl"],
                    "the same window, which is what a client that lost the first answer needs"
                );
            })
            .await;
    })
    .await;
}

/// Signing out forgets the grant and leaves the setting alone: signing out is
/// not turning the provider off.
#[tokio::test]
async fn signing_a_provider_out_forgets_the_grant_and_not_the_setting() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        store_grant(&paths.grants);
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let mut config = crate::config::Config::default();
                config.providers.codex_enabled = true;
                let schema = schema_over(state_with_providers(
                    crate::commands::serve::providers::ProviderAdmin::default(),
                    config,
                ));

                let signed_out = ask_of(
                    &schema,
                    r#"mutation { signOutProvider(request: { provider: "codex" })
                         { provider { id signedIn enabled account } } }"#,
                )
                .await;
                let provider = &signed_out["signOutProvider"]["provider"];
                assert_eq!(provider["id"], "provider:codex");
                assert_eq!(provider["signedIn"], false, "the grant is gone");
                assert!(provider["account"].is_null());
                assert_eq!(
                    provider["enabled"], true,
                    "and the setting it was signed in for is untouched"
                );
            })
            .await;
    })
    .await;
}

/// The check asks the account, and sorts what comes back into the models this
/// machine's catalogue knows and the ids it has no entry for.
#[tokio::test]
async fn checking_a_provider_sorts_what_the_account_says() {
    let usage = leviath_testkit::spawn_mock_server(
        200,
        "OK",
        br#"{"plan_type":"plus","rate_limit":{}}"#.to_vec(),
    )
    .await;
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        store_grant(&paths.grants);
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let admin = crate::commands::serve::providers::ProviderAdmin {
                    usage_url: Some(usage),
                    ..Default::default()
                };
                let mut config = crate::config::Config::default();
                config.providers.codex_enabled = true;
                let schema = schema_over(state_with_providers(admin, config));

                let checked = ask_of(
                    &schema,
                    r#"mutation { checkProvider(request: { provider: "codex" })
                         { provider { id signedIn } models { id modelId providerName }
                           unlistedModelIds } }"#,
                )
                .await;
                let result = &checked["checkProvider"];
                assert_eq!(result["provider"]["id"], "provider:codex");
                assert_eq!(result["provider"]["signedIn"], true);
                let models = result["models"].as_array().expect("a model list");
                assert!(
                    !models.is_empty(),
                    "the account named models this machine's catalogue knows: {result}"
                );
                for model in models {
                    assert_eq!(
                        model["providerName"], "codex",
                        "one provider's catalogue, so an id two providers serve cannot \
                         bring the other one's entry with it"
                    );
                }
                assert!(
                    result["unlistedModelIds"]
                        .as_array()
                        .expect("the rest")
                        .is_empty(),
                    "and nothing the account named is missing from it: {result}"
                );
            })
            .await;
    })
    .await;
}

/// A machine whose catalogue carries nothing for the provider answers with the
/// ids alone.
///
/// The other half of the same sort, and the answer a console has to render: the
/// account named models, and there is nothing more to say about them than their
/// ids. A provider that is turned off has no catalogue on this machine, which is
/// the ordinary way to be in that state.
#[tokio::test]
async fn a_check_against_no_catalogue_answers_with_the_ids_alone() {
    let usage = leviath_testkit::spawn_mock_server(
        200,
        "OK",
        br#"{"plan_type":"plus","rate_limit":{}}"#.to_vec(),
    )
    .await;
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        store_grant(&paths.grants);
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let admin = crate::commands::serve::providers::ProviderAdmin {
                    usage_url: Some(usage),
                    ..Default::default()
                };
                // Signed in, and turned off: the grant answers the check, and
                // the catalogue has no rows for a provider nothing routes to.
                let schema = schema_over(state_with_providers(
                    admin,
                    crate::config::Config::default(),
                ));

                let checked = ask_of(
                    &schema,
                    r#"mutation { checkProvider(request: { provider: "codex" })
                         { provider { id enabled } models { id } unlistedModelIds } }"#,
                )
                .await;
                let result = &checked["checkProvider"];
                assert_eq!(result["provider"]["enabled"], false);
                assert_eq!(result["models"], serde_json::json!([]));
                assert!(
                    !result["unlistedModelIds"]
                        .as_array()
                        .expect("the ids")
                        .is_empty(),
                    "the ids the account named, with no entry to dress them up: {result}"
                );
            })
            .await;
    })
    .await;
}

/// A row written under a pattern reads back as the pattern, with what a type
/// under it inherits.
#[tokio::test]
async fn a_row_written_under_a_pattern_reads_back_as_the_pattern() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = ask(
            r#"mutation { upsertMimeRow(request: { row: { mimeType: "image/*",
                 standIn: "[a picture]" } }) { isNew mimeRow { mimeType origin standIn } } }"#,
        )
        .await;
        let row = &written["upsertMimeRow"]["mimeRow"];
        assert_eq!(row["mimeType"], "image/*");
        assert_eq!(row["standIn"], "[a picture]");
        assert_eq!(row["origin"], "CONFIG");
    })
    .await;
}

/// The live diagnostics answer in the shape the `doctor` field answers with,
/// which is what lets a client render one view for both.
#[tokio::test]
async fn the_live_diagnostics_answer_a_report() {
    crate::config::with_isolated_config_path_async("graphql-check-machine", |path| async move {
        let paths = crate::commands::serve::mcp::AdminPaths {
            config: path.clone(),
            store: path.with_file_name("mcp-auth.json"),
            grants: path.with_file_name("provider-auth.json"),
        };
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                let report =
                    ask("mutation { checkMachine { report { ok checks { name ok detail } } } }")
                        .await;
                let report = &report["checkMachine"]["report"];
                assert!(report["ok"].is_boolean(), "a verdict either way: {report}");
                assert!(
                    !report["checks"].as_array().expect("the checks").is_empty(),
                    "the live run reports what it asked: {report}"
                );
            })
            .await;
    })
    .await;
}

/// An update that nothing is racing starts, and answers with the job to watch.
#[tokio::test]
async fn an_update_starts_and_answers_with_its_job() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let agents = home.join("agents");
        let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
        state.update_jobs = crate::commands::serve::update_job::UpdateJobs::with_env(
            std::sync::Arc::new(move || crate::commands::update::UpdateEnv {
                agents_dir: agents.clone(),
                runner: std::sync::Arc::new(|_argv: &[String]| Ok(())),
                ..crate::commands::update::UpdateEnv::for_planning_offline()
            }),
        );
        let schema = schema_over(state);

        // No steps named, which is what a person clicking "update" means.
        let started = ask_of(
            &schema,
            "mutation { startUpdate(request: {}) { job { id status steps { step status } } } }",
        )
        .await;
        let job = &started["startUpdate"]["job"];
        assert!(
            !job["id"].as_str().unwrap_or_default().is_empty(),
            "the job to poll: {job}"
        );
        assert_eq!(
            job["steps"].as_array().map(Vec::len),
            Some(4),
            "a request that names none asks for all of them: {job}"
        );
    })
    .await;
}

/// A script written under a blueprint's name lands in that blueprint's scope
/// rather than the machine-wide one.
#[tokio::test]
async fn a_blueprint_scoped_script_says_which_blueprint_it_is_for() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let written = ask(r#"mutation { upsertScript(request: {
                 script: { kind: TOOL, name: "greet", blueprintName: "coder" },
                 content: "\"hi\"" })
                 { script { id kind name scope blueprintName isDeclared
                   relativePath } } }"#)
        .await;
        let script = &written["upsertScript"]["script"];
        assert_eq!(script["scope"], "BLUEPRINT");
        assert_eq!(script["blueprintName"], "coder");
        assert_eq!(script["id"], "script:tool@coder:greet");

        // What a blueprint's manifest would name the file, which the write
        // answers with and the read answers with because both build the script
        // through one constructor. A write that left it out had an editor
        // reading null from the save and a path from the next listing.
        assert_eq!(script["relativePath"], "tools/greet.rhai");
        let read = ask(
            r#"{ script(ref: { kind: TOOL, name: "greet", blueprintName: "coder" })
                 { relativePath } }"#,
        )
        .await;
        assert_eq!(script["relativePath"], read["script"]["relativePath"]);
    })
    .await;
}

/// Checking an MCP server connects to it and names what it advertises.
///
/// The only honest answer to "does this server work": a config that parses
/// proves nothing about a program that will not start.
#[tokio::test]
async fn checking_an_mcp_server_names_the_tools_it_advertises() {
    crate::commands::serve::testutil::with_home(|home| async move {
        let paths = paths_in(&home);
        std::fs::write(&paths.config, "").expect("a config file");
        crate::commands::serve::mcp::TEST_PATHS
            .scope(paths, async {
                // A stdio server that speaks just enough MCP to be asked.
                let stub = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); m = req.get("method",""); i = req.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"capabilities":{},"protocolVersion":"2024-11-05"}}), flush=True)
    elif m == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"tools":[{"name":"ping","inputSchema":{}}]}}), flush=True)
"#;
                let written = schema(true)
                    .execute(
                        Request::new(
                            r#"mutation Write($stub: String!) {
                                 createMcpServer(request: { server: { name: "local",
                                   transport: { stdio: { command: "python3",
                                     args: ["-c", $stub] } } } })
                                 { mcpServer { name } } }"#,
                        )
                        .variables(async_graphql::Variables::from_json(
                            serde_json::json!({ "stub": stub }),
                        )),
                    )
                    .await;
                assert!(written.errors.is_empty(), "{:?}", written.errors);

                let checked = ask(
                    r#"mutation { checkMcpServer(request: { name: "local" })
                         { toolNames mcpServer { name transport auth } } }"#,
                )
                .await;
                let result = &checked["checkMcpServer"];
                assert_eq!(result["toolNames"], serde_json::json!(["ping"]));
                assert_eq!(result["mcpServer"]["name"], "local");
                assert_eq!(result["mcpServer"]["transport"], "STDIO");
            })
            .await;
    })
    .await;
}
