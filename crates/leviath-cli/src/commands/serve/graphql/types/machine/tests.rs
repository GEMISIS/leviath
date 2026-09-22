//! Tests for the machine-side types that read a word and answer a value.
//!
//! The server descriptions these read from are shared with the REST routes, so
//! the words are fixed there and the mapping is what this schema adds. What is
//! asserted is that every word those routes can carry has a value here, and
//! that a word none of them carries lands somewhere safe rather than panicking.

use async_graphql::ID;

use super::{
    CodexOptions, CodexReasoningEffort, CodexVerbosity, Config, ConfigError, ConfigErrorKind,
    ConfigFileStatus, ConfigHealth, Directory, DoctorCheck, DoctorReport, Fixed, Gateway,
    GatewayKind, JournalHealth, JournalWriteError, McpAuth, McpServer, McpServerOrderField,
    MimeRow, MimeRowOrderField, MimeRowOrigin, MimeTokenRule, PerByte, PerPage, PerPixel,
    PerSecond, ProviderAuthKind, ProviderConfig, ProviderOptions, RoutingConfig, Script,
    ScriptOrderField, ServeLimits, ServerInfo, ShellRule, YoloHuman, YoloProfile,
    YoloProfileOrderField, YoloShellRules, YoloToolRules, YoloWaiver, transport_of,
};
use crate::commands::serve::graphql::filter::testkit::{
    exercise, exercise_enum, exercise_list, exercise_order,
};
use crate::commands::serve::graphql::scalars::{BigInt, Timestamp};
use crate::commands::serve::graphql::script_ref::{ScriptKind, ScriptScope};
use crate::commands::serve::graphql::types::manifest::dependency::McpTransport;

/// Every transport word the server description carries, and the two it never
/// resolves for.
///
/// "Neither" is the absence of a transport rather than a third one, so it is a
/// null here and `configError` beside it says why.
#[test]
fn every_transport_word_reads_back() {
    assert_eq!(transport_of("stdio"), Some(McpTransport::Stdio));
    assert_eq!(transport_of("http"), Some(McpTransport::Http));
    assert_eq!(transport_of("invalid"), None);
    // A word this build does not know is a transport it cannot use, which is
    // the same answer.
    assert_eq!(transport_of("carrier-pigeon"), None);
}

/// Every `source` word a registry row carries, and what a name reads as.
#[test]
fn every_mime_source_word_reads_back() {
    assert_eq!(
        MimeRow::origin_of("builtin".to_string()),
        (MimeRowOrigin::Builtin, None)
    );
    assert_eq!(
        MimeRow::origin_of("config".to_string()),
        (MimeRowOrigin::Config, None)
    );
    // The operator's two layers are one origin: the `[mime_types]` table and
    // the file beside it are both the machine's own configuration, and a row
    // written through the API lands in the file.
    assert_eq!(
        MimeRow::origin_of(crate::config::MIME_TYPES_FILE.to_string()),
        (MimeRowOrigin::Config, None)
    );
    assert_eq!(
        MimeRow::origin_of("provider:meshy".to_string()),
        (MimeRowOrigin::Builtin, None),
        "a provider's types ship with its script, not with a blueprint"
    );
    assert_eq!(
        MimeRow::origin_of("sprite-to-3d".to_string()),
        (MimeRowOrigin::Blueprint, Some("sprite-to-3d".to_string()))
    );
}

/// Every auth word, and the reading an unknown one gets.
#[test]
fn every_auth_word_reads_back() {
    assert_eq!(McpAuth::from_wire("n/a"), McpAuth::NotApplicable);
    assert_eq!(McpAuth::from_wire("none"), McpAuth::None);
    assert_eq!(McpAuth::from_wire("header"), McpAuth::Header);
    assert_eq!(McpAuth::from_wire("authenticated"), McpAuth::Authenticated);
    assert_eq!(McpAuth::from_wire("expired"), McpAuth::Expired);
    // `NONE` rather than anything else: it is the reading that offers a login
    // instead of assuming a credential is already in place.
    assert_eq!(McpAuth::from_wire("who knows"), McpAuth::None);
}

/// Every word the config health snapshot carries, and one from a newer daemon.
#[test]
fn every_config_error_word_reads_back() {
    assert_eq!(ConfigErrorKind::from_wire("read"), ConfigErrorKind::Read);
    assert_eq!(ConfigErrorKind::from_wire("parse"), ConfigErrorKind::Parse);
    assert_eq!(
        ConfigErrorKind::from_wire("validation"),
        ConfigErrorKind::Validate
    );
    assert_eq!(
        ConfigErrorKind::from_wire("tectonic"),
        ConfigErrorKind::Unknown,
        "a word this build has no step for is one it cannot name"
    );
}

/// Every gateway kind the config file spells, both ways, and the reading an
/// unknown word gets.
#[test]
fn every_gateway_kind_reads_back() {
    for kind in [
        GatewayKind::Script,
        GatewayKind::OpenaiCompatible,
        GatewayKind::Openai,
    ] {
        assert_eq!(GatewayKind::from_wire(kind.as_wire()), kind);
    }
    assert_eq!(
        GatewayKind::from_wire("semaphore"),
        GatewayKind::Script,
        "an entry with no kind this build knows is a script, as in the file"
    );
}

/// Every Codex word the config file can hold, and one the provider would
/// ignore.
#[test]
fn every_codex_word_reads_back() {
    for effort in [
        CodexReasoningEffort::None,
        CodexReasoningEffort::Minimal,
        CodexReasoningEffort::Low,
        CodexReasoningEffort::Medium,
        CodexReasoningEffort::High,
        CodexReasoningEffort::Xhigh,
    ] {
        assert_eq!(
            CodexReasoningEffort::from_wire(effort.as_wire()),
            Some(effort)
        );
    }
    assert_eq!(CodexReasoningEffort::from_wire("titanic"), None);

    for verbosity in [
        CodexVerbosity::Low,
        CodexVerbosity::Medium,
        CodexVerbosity::High,
    ] {
        assert_eq!(
            CodexVerbosity::from_wire(verbosity.as_wire()),
            Some(verbosity)
        );
    }
    assert_eq!(CodexVerbosity::from_wire("shouty"), None);
}

/// Every rule the registry can hold becomes the union member that says it.
#[test]
fn every_token_rule_becomes_its_own_member() {
    use leviath_core::mime::registry::TokenRule as Core;

    let cases = [
        (Core::PerByte(0.25), "bytes"),
        (
            Core::PerPixel {
                divisor: 750,
                max: 1600,
            },
            "pixels",
        ),
        (Core::PerSecond(50), "seconds"),
        (Core::PerPage(260), "pages"),
        (Core::Fixed(500), "flat"),
    ];
    for (rule, expected) in cases {
        let named = match MimeTokenRule::from(&rule) {
            MimeTokenRule::Bytes(bytes) => {
                assert!((bytes.tokens_per_byte - 0.25).abs() < f64::EPSILON);
                "bytes"
            }
            MimeTokenRule::Pixels(pixels) => {
                assert_eq!(pixels.pixels_per_token, 750);
                assert_eq!(pixels.max, 1600);
                "pixels"
            }
            MimeTokenRule::Seconds(seconds) => {
                assert_eq!(seconds.tokens_per_second, 50);
                "seconds"
            }
            MimeTokenRule::Pages(pages) => {
                assert_eq!(pages.tokens_per_page, 260);
                "pages"
            }
            MimeTokenRule::Flat(flat) => {
                assert_eq!(flat.tokens, 500);
                "flat"
            }
        };
        assert_eq!(named, expected);
    }
}

/// The rows in a `[mime_types]` table come back sorted by mime type, carrying
/// the name of whatever shipped them, and a row that will not deserialize is
/// left out rather than reported as empty.
#[test]
fn from_table_reads_every_row_that_parses_sorted_by_type() {
    let table: toml::Table = toml::from_str(
        "[\"model/gltf+json\"]\n\
         family = \"model\"\n\
         text = true\n\
         extensions = [\"gltf\"]\n\
         magic = \"67 6c 54 46\"\n\
         stand_in = \"a 3D model\"\n\
         check = \"checks/gltf.rhai\"\n\
         tokens = { fixed = 500 }\n\n\
         [\"a/first\"]\n\
         family = \"other\"\n\n\
         [\"broken/thing\"]\n\
         family = 7\n",
    )
    .expect("the table parses");
    let rows = MimeRow::from_table(&table, "sculptor");
    assert_eq!(rows.len(), 2, "the row that will not read is left out");
    assert_eq!(rows[0].mime_type, "a/first", "sorted by mime type");
    let gltf = &rows[1];
    assert_eq!(gltf.mime_type, "model/gltf+json");
    assert_eq!(gltf.origin, MimeRowOrigin::Blueprint);
    assert_eq!(
        gltf.blueprint_name,
        Some("sculptor".to_string()),
        "the blueprint that ships it"
    );
    assert_eq!(gltf.family, Some("model".to_string()));
    assert_eq!(gltf.is_text, Some(true));
    assert_eq!(gltf.extensions, vec!["gltf".to_string()]);
    assert_eq!(gltf.magic, Some("67 6c 54 46".to_string()));
    assert_eq!(gltf.stand_in, Some("a 3D model".to_string()));
    assert_eq!(gltf.check, Some("checks/gltf.rhai".to_string()));
    let Some(MimeTokenRule::Flat(flat)) = gltf.tokens.as_ref() else {
        panic!("a flat charge, as the table says");
    };
    assert_eq!(flat.tokens, 500);
}

/// Every function `#[mirror]` wrote for this directory's types runs at least
/// once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them.
#[tokio::test]
async fn every_mirrored_function_runs() {
    fn limits() -> ServeLimits {
        ServeLimits {
            max_page_size: 100,
            max_ids: 50,
            max_file_bytes: BigInt(1_000_000),
            max_listing_entries: 200,
            max_search_scan: 500,
            max_history_limit: 100,
            max_concurrent_requests: BigInt(16),
            max_upload_bytes: BigInt(10_000_000),
            request_timeout_secs: 30,
        }
    }
    fn config_error() -> ConfigError {
        ConfigError {
            kind: ConfigErrorKind::Parse,
            path: "/etc/leviath/config.toml".to_string(),
            message: "unexpected token".to_string(),
            line: Some(4),
            column: Some(9),
            key: Some("default_provider".to_string()),
            since: Timestamp(1_788_000_000),
            note: "the config did not parse".to_string(),
        }
    }
    fn gateway() -> Gateway {
        Gateway {
            name: "mine".to_string(),
            kind: GatewayKind::OpenaiCompatible,
            base_url: Some("https://gateway.example".to_string()),
            script: None,
            has_api_key: true,
            header_names: vec!["X-Thing".to_string()],
            models: vec!["llama".to_string()],
            unknown_keys: vec!["signing_secret".to_string()],
        }
    }
    fn provider() -> ProviderConfig {
        ProviderConfig {
            id: ID::from("codex"),
            name: "Codex".to_string(),
            auth: ProviderAuthKind::SignIn,
            is_enabled: true,
            has_key: false,
            base_url: None,
            region: None,
            options: Some(ProviderOptions::Codex(CodexOptions {
                reasoning_effort: Some(CodexReasoningEffort::High),
                verbosity: Some(CodexVerbosity::Low),
                replays_reasoning: true,
            })),
        }
    }
    fn check() -> DoctorCheck {
        DoctorCheck {
            name: "ffmpeg".to_string(),
            ok: true,
            detail: "found on PATH".to_string(),
        }
    }
    fn server() -> McpServer {
        McpServer {
            id: ID::from("mcpServer:search"),
            name: "search".to_string(),
            transport: Some(McpTransport::Stdio),
            endpoint: "search-mcp".to_string(),
            command: Some("search-mcp".to_string()),
            url: None,
            args: vec!["--fast".to_string()],
            header_names: Vec::new(),
            env_names: vec!["SEARCH_TOKEN".to_string()],
            config_error: None,
            auth: McpAuth::NotApplicable,
        }
    }
    fn profile() -> YoloProfile {
        YoloProfile {
            id: ID::from("yoloProfile:default"),
            name: "default".to_string(),
            default: YoloWaiver::Allow,
            questions: YoloHuman::Auto,
            checkpoints: YoloHuman::Ask,
            gate: YoloHuman::Ask,
            tool_rules: YoloToolRules {
                allow: vec!["read_file".to_string(), "@builtin".to_string()],
                ask: vec!["web_fetch".to_string()],
                deny: Vec::new(),
            },
            shell_rules: YoloShellRules {
                allow: vec![ShellRule {
                    command: "cargo *".to_string(),
                    args: None,
                }],
                ask: Vec::new(),
                deny: vec![ShellRule {
                    command: "rm -r*".to_string(),
                    args: Some(vec!["/tmp/*".to_string()]),
                }],
            },
        }
    }
    fn row() -> MimeRow {
        MimeRow {
            mime_type: "image/png".to_string(),
            origin: MimeRowOrigin::Builtin,
            blueprint_name: None,
            family: Some("image".to_string()),
            is_text: Some(false),
            tokens: Some(MimeTokenRule::Pixels(PerPixel {
                pixels_per_token: 750,
                max: 1600,
            })),
            extensions: vec!["png".to_string()],
            magic: Some("89504e47".to_string()),
            stand_in: Some("[a picture]".to_string()),
            check: None,
        }
    }
    fn script() -> Script {
        Script {
            id: ID::from("script:tool:summarise"),
            kind: ScriptKind::Tool,
            name: "summarise".to_string(),
            scope: ScriptScope::Global,
            blueprint_name: None,
            path: "/home/user/.leviath/tools/summarise.rhai".to_string(),
            relative_path: None,
            is_declared: true,
            compiles: Some(true),
            compile_error: None,
        }
    }
    fn write_error() -> JournalWriteError {
        JournalWriteError {
            run_id: ID::from("run-1"),
            path: "/runs/run-1/journal".to_string(),
            message: "disk full".to_string(),
            at: Timestamp(1_788_000_200),
        }
    }

    exercise(&[limits()]).await;
    exercise(&[config_error()]).await;
    exercise_enum(&[ConfigErrorKind::Parse, ConfigErrorKind::Unknown]).await;
    exercise(&[gateway()]).await;
    exercise_list(&[gateway()]).await;
    exercise_enum(&[GatewayKind::Script, GatewayKind::Openai]).await;
    exercise(&[provider()]).await;
    exercise_list(&[provider()]).await;
    exercise_enum(&[ProviderAuthKind::ApiKey, ProviderAuthKind::None]).await;
    exercise_enum(&[CodexReasoningEffort::High, CodexReasoningEffort::Xhigh]).await;
    exercise_enum(&[CodexVerbosity::Low, CodexVerbosity::High]).await;
    exercise(&[CodexOptions {
        reasoning_effort: None,
        verbosity: None,
        replays_reasoning: false,
    }])
    .await;
    exercise(&[ProviderOptions::Codex(CodexOptions {
        reasoning_effort: Some(CodexReasoningEffort::Medium),
        verbosity: Some(CodexVerbosity::Medium),
        replays_reasoning: true,
    })])
    .await;
    exercise(&[RoutingConfig {
        default_provider: "openai".to_string(),
        provider_order: vec!["openai".to_string()],
        override_model: Some("gpt-5.6".to_string()),
        fallback_model: None,
    }])
    .await;
    exercise(&[ServerInfo {
        api_version: "v1".to_string(),
        capabilities: vec!["providers.quota".to_string()],
        is_admin_enabled: true,
        limits: limits(),
    }])
    .await;
    exercise(&[ConfigHealth {
        error: Some(config_error()),
        saved_at: Some(Timestamp(1_788_000_100)),
    }])
    .await;
    exercise(&[ConfigFileStatus {
        path: "~/.leviath/yolo.toml".to_string(),
        exists: true,
        error: None,
    }])
    .await;

    exercise(&[Config {
        routing: RoutingConfig {
            default_provider: "openai".to_string(),
            provider_order: vec!["openai".to_string()],
            override_model: Some("gpt-5.6".to_string()),
            fallback_model: None,
        },
        providers: vec![provider()],
        gateways: vec![gateway()],
        allows_file_uploads: true,
        blueprint_paths: vec!["~/.leviath/agents".to_string()],
        mcp_server_count: 2,
        server: ServerInfo {
            api_version: "v1".to_string(),
            capabilities: vec!["providers.quota".to_string()],
            is_admin_enabled: true,
            limits: limits(),
        },
        health: ConfigHealth {
            error: Some(config_error()),
            saved_at: Some(Timestamp(1_788_000_100)),
        },
        yolo_file: ConfigFileStatus {
            path: "~/.leviath/yolo.toml".to_string(),
            exists: false,
            error: None,
        },
    }])
    .await;

    exercise(&[check()]).await;
    exercise_list(&[check()]).await;
    exercise(&[DoctorReport {
        ok: true,
        is_live: false,
        checks: vec![check()],
    }])
    .await;

    exercise(&[server()]).await;
    exercise_list(&[server()]).await;
    exercise_order(&server(), McpServerOrderField::ALL);
    exercise_enum(&[McpAuth::NotApplicable, McpAuth::Authenticated]).await;

    exercise(&[profile()]).await;
    exercise_list(&[profile()]).await;
    exercise_order(&profile(), YoloProfileOrderField::ALL);
    exercise_enum(&[YoloWaiver::Allow, YoloWaiver::Ask]).await;
    exercise_enum(&[YoloHuman::Ask, YoloHuman::Auto]).await;
    exercise(&[profile().tool_rules]).await;
    exercise(&[profile().shell_rules]).await;
    exercise(&[ShellRule {
        command: "cargo build*".to_string(),
        args: None,
    }])
    .await;
    exercise_list(&[ShellRule {
        command: "cargo build*".to_string(),
        args: Some(vec!["--release".to_string()]),
    }])
    .await;

    exercise(&[PerByte {
        tokens_per_byte: 0.25,
    }])
    .await;
    exercise(&[PerPixel {
        pixels_per_token: 750,
        max: 1600,
    }])
    .await;
    exercise(&[PerSecond {
        tokens_per_second: 50,
    }])
    .await;
    exercise(&[PerPage {
        tokens_per_page: 260,
    }])
    .await;
    exercise(&[Fixed { tokens: 500 }]).await;
    exercise(&[
        MimeTokenRule::Bytes(PerByte {
            tokens_per_byte: 0.25,
        }),
        MimeTokenRule::Pixels(PerPixel {
            pixels_per_token: 750,
            max: 1600,
        }),
        MimeTokenRule::Seconds(PerSecond {
            tokens_per_second: 50,
        }),
        MimeTokenRule::Pages(PerPage {
            tokens_per_page: 260,
        }),
        MimeTokenRule::Flat(Fixed { tokens: 500 }),
    ])
    .await;

    exercise(&[row()]).await;
    exercise_list(&[row()]).await;
    exercise_order(&row(), MimeRowOrderField::ALL);
    exercise_enum(&[MimeRowOrigin::Builtin, MimeRowOrigin::Blueprint]).await;

    exercise(&[script()]).await;
    exercise_list(&[script()]).await;
    exercise_order(&script(), ScriptOrderField::ALL);

    exercise(&[write_error()]).await;
    exercise(&[JournalHealth {
        healthy: false,
        appends_attempted: BigInt(10),
        appends_failed: BigInt(1),
        snapshots_failed: BigInt(0),
        queue_depth: 3,
        last_error: Some(write_error()),
    }])
    .await;

    exercise(&[Directory {
        path: "/home/user/projects".to_string(),
        parent: Some("/home/user".to_string()),
        home: "/home/user".to_string(),
        cwd: "/home/user/projects".to_string(),
        entries: vec!["a".to_string(), "b".to_string()],
    }])
    .await;
}

/// A profile that is no longer in the file cannot decide, and says so.
///
/// A profile object is read once and asked afterwards: the decision goes back
/// to the file for the compiled rules, so a file edited between the two is the
/// one moment the rules a caller is holding are not there any more.
#[tokio::test]
async fn a_profile_the_file_no_longer_holds_cannot_decide() {
    use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

    /// A root handing out one profile, whatever the file says.
    struct Probe {
        profile: YoloProfile,
    }

    #[async_graphql::Object]
    impl Probe {
        /// The profile under test.
        async fn profile(&self) -> &YoloProfile {
            &self.profile
        }
    }

    crate::commands::serve::testutil::with_home(|_home| async move {
        let schema = Schema::build(
            Probe {
                profile: YoloProfile {
                    id: ID::from("yoloProfile:gone"),
                    name: "gone".to_string(),
                    default: YoloWaiver::Ask,
                    questions: YoloHuman::Ask,
                    checkpoints: YoloHuman::Ask,
                    gate: YoloHuman::Ask,
                    tool_rules: YoloToolRules {
                        allow: Vec::new(),
                        ask: Vec::new(),
                        deny: Vec::new(),
                    },
                    shell_rules: YoloShellRules {
                        allow: Vec::new(),
                        ask: Vec::new(),
                        deny: Vec::new(),
                    },
                },
            },
            EmptyMutation,
            EmptySubscription,
        )
        .data(crate::commands::serve::testutil::state_with_agent_paths(
            Vec::new(),
        ))
        .finish();
        let answer = schema
            .execute(Request::new(
                r#"{ profile { decide(tool: "read_file", kind: BUILTIN)
                     { policy } } }"#,
            ))
            .await;
        assert!(
            answer
                .errors
                .first()
                .is_some_and(|error| error.message.contains("gone")),
            "{:?}",
            answer.errors
        );
    })
    .await;
}
