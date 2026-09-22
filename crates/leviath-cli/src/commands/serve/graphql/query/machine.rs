//! The `config`, `doctor`, `mcpServers`, `yoloProfiles`, `mimeRows`,
//! `scripts`, `directory`, `daemon` and `updatePlan` fields: everything a
//! settings screen or a diagnostics view asks about the machine this server
//! runs on, rather than about a run.

use async_graphql::Context;

use super::super::super::types::AppState;
use super::super::connection::Connection;
use super::super::error::IntoGraphql;
use super::super::paging::order::{OrderDirection, Term};
use super::super::scalars::{BigInt, Cursor, Timestamp};
use super::super::script_ref::ScriptRef;
use super::super::types::machine::{
    CodexOptions, CodexReasoningEffort, CodexVerbosity, Config, ConfigError, ConfigErrorKind,
    ConfigFileStatus, ConfigHealth, Directory, DoctorCheck, DoctorReport, Gateway, GatewayKind,
    McpServer, McpServerFilter, McpServerOrder, McpServerOrderField, MimeRow, MimeRowFilter,
    MimeRowOrder, MimeRowOrderField, ProviderAuthKind, ProviderConfig, ProviderOptions,
    RoutingConfig, Script, ScriptFilter, ScriptOrder, ScriptOrderField, ServeLimits, ServerInfo,
    YoloProfile, YoloProfileFilter, YoloProfileOrder, YoloProfileOrderField,
};
use super::super::types::update::{DaemonStatus, UpdatePlan};
use super::listing::{Window, connection, terms};

/// How many rows one of this file's listings may carry in a page.
///
/// These are the operator's own configuration rather than anything that
/// accumulates, so the cap is about a client that meant to page and did not.
const PAGE_CAP: usize = 200;

/// How this server is configured, with every secret left out.
///
/// Read `capabilities` before choosing a code path. A 404 also means "no
/// such run", so discovering a feature by being refused costs a round trip
/// and tells you less.
pub(crate) async fn config(ctx: &Context<'_>) -> Config {
    let state = ctx.data_unchecked::<AppState>();
    // One health read rather than a config read beside it: health re-checks
    // the file and hands back the config in force with its verdict, so the
    // two halves of one answer cannot disagree.
    let health = state.config.health();
    config_of(
        &health.config.clone(),
        &state.limits.request_limits,
        &health,
        super::super::admin::admin_visible(ctx),
    )
}

/// Environment and configuration diagnostics.
///
/// A failing check is `ok: false` inside a healthy answer, never an error:
/// the request to run the checks succeeded, and what they found is the
/// answer.
pub(crate) async fn doctor() -> DoctorReport {
    let report = super::super::super::doctor::offline_report().await;
    doctor_report(report.checks)
}

/// The MCP servers a machine's configuration names, parsed.
async fn configured_servers(ctx: &Context<'_>) -> async_graphql::Result<Vec<McpServer>> {
    let state = ctx.data_unchecked::<AppState>();
    Ok(super::super::super::mcp::server_infos(state)
        .gql()?
        .into_iter()
        .map(McpServer::from_info)
        .collect())
}

/// The order the MCP servers are read in when nothing says otherwise: by name,
/// which is the key a machine holds one server per.
fn servers_by_name() -> Vec<Term<McpServerOrderField>> {
    vec![Term {
        field: McpServerOrderField::Name,
        direction: OrderDirection::Asc,
    }]
}

/// The MCP servers this machine has configured.
pub(crate) async fn mcp_servers(
    ctx: &Context<'_>,
    filter: Option<McpServerFilter>,
    order_by: Option<Vec<McpServerOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<McpServer>> {
    connection(
        configured_servers(ctx).await?,
        filter,
        terms(order_by, McpServerOrder::term, servers_by_name),
        |server: &McpServer| server.name.clone(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the MCP server page cap",
        },
    )
    .await
}

/// One configured MCP server, by the name it is configured under.
pub(crate) async fn mcp_server(
    ctx: &Context<'_>,
    name: String,
) -> async_graphql::Result<Option<McpServer>> {
    Ok(configured_servers(ctx)
        .await?
        .into_iter()
        .find(|server| server.name == name))
}

/// The operator's mime registry, before any blueprint's own rows.
fn registry_rows(ctx: &Context<'_>) -> Vec<MimeRow> {
    let state = ctx.data_unchecked::<AppState>();
    super::super::super::blobs::mime_rows(state)
        .into_iter()
        .map(MimeRow::from_entry)
        .collect()
}

/// The order the registry is read in when nothing says otherwise: by key,
/// which is how the registry itself is stored and how a person reads it.
fn rows_by_type() -> Vec<Term<MimeRowOrderField>> {
    vec![Term {
        field: MimeRowOrderField::MimeType,
        direction: OrderDirection::Asc,
    }]
}

/// The operator's mime registry, before any blueprint's own rows.
pub(crate) async fn mime_rows(
    ctx: &Context<'_>,
    filter: Option<MimeRowFilter>,
    order_by: Option<Vec<MimeRowOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<MimeRow>> {
    connection(
        registry_rows(ctx),
        filter,
        terms(order_by, MimeRowOrder::term, rows_by_type),
        |row: &MimeRow| row.mime_type.clone(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the mime row page cap",
        },
    )
    .await
}

/// Every script registered on this machine, and in one blueprint's own
/// directory when `blueprint` names one.
pub(crate) async fn registered_scripts(
    ctx: &Context<'_>,
    blueprint: Option<&str>,
) -> async_graphql::Result<Vec<Script>> {
    let state = ctx.data_unchecked::<AppState>();
    Ok(super::super::super::scripts::registered(state, blueprint)
        .gql()?
        .into_iter()
        .map(Script::from_item)
        .collect())
}

/// The order the scripts are read in when nothing says otherwise: by id, which
/// carries the kind and the owning blueprint as well as the name, so the kinds
/// stay together.
fn scripts_by_id() -> Vec<Term<ScriptOrderField>> {
    vec![Term {
        field: ScriptOrderField::Id,
        direction: OrderDirection::Asc,
    }]
}

/// The scripts this machine has registered.
pub(crate) async fn scripts(
    ctx: &Context<'_>,
    filter: Option<ScriptFilter>,
    order_by: Option<Vec<ScriptOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Script>> {
    script_page(ctx, None, filter, order_by, first, after).await
}

/// One page of the scripts one scope can see.
///
/// Shared with `BlueprintOutput.scripts`, which is the same listing with the
/// blueprint's own directory walked as well.
pub(crate) async fn script_page(
    ctx: &Context<'_>,
    blueprint: Option<&str>,
    filter: Option<ScriptFilter>,
    order_by: Option<Vec<ScriptOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Script>> {
    connection(
        registered_scripts(ctx, blueprint).await?,
        filter,
        terms(order_by, ScriptOrder::term, scripts_by_id),
        |script: &Script| script.id.to_string(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the script page cap",
        },
    )
    .await
}

/// One registered script, by the three things that name it.
///
/// Null for a reference nothing is filed under, which is what a client that
/// guessed a kind or a blueprint gets rather than an error.
pub(crate) async fn script(
    ctx: &Context<'_>,
    reference: ScriptRef,
) -> async_graphql::Result<Option<Script>> {
    let blueprint = reference.blueprint_name.clone();
    let found = registered_scripts(ctx, blueprint.as_deref()).await?;
    Ok(found.into_iter().find(|script| {
        script.kind == reference.kind
            && script.name == reference.name
            && script.blueprint_name == blueprint
    }))
}

/// The directories under a path, for a file picker.
///
/// Confined to `--workdir-root` when the operator set one, which is also
/// why `parent` is null at that fence rather than leading above it.
pub(crate) async fn directory(
    ctx: &Context<'_>,
    path: Option<String>,
    include_hidden: bool,
) -> async_graphql::Result<Directory> {
    let state = ctx.data_unchecked::<AppState>();
    let listing =
        super::super::super::fs::dir_listing(state, path.as_deref(), include_hidden).gql()?;
    Ok(Directory {
        path: listing.path,
        parent: listing.parent,
        home: listing.home,
        cwd: listing.cwd,
        entries: listing.dirs.into_iter().map(|dir| dir.name).collect(),
    })
}

/// Who is on the other end of the control socket.
///
/// A read this server answers from what it already knows, so it works while
/// the daemon is down: that is the point of asking. `connected` false does
/// not mean requests fail, it means the live frames have stopped.
pub(crate) async fn daemon(ctx: &Context<'_>) -> DaemonStatus {
    let state = ctx.data_unchecked::<AppState>();
    DaemonStatus::of(state.control.link(), state.control.code_mismatch())
}

/// What an update would do, and whether there is anything newer to get.
///
/// Planning never reaches the network. The "is there anything newer" half is
/// whatever the last check found, and asking starts another one for whoever
/// asks next rather than waiting on one here, so this is cheap enough for a
/// page to ask every time it opens.
pub(crate) async fn update(ctx: &Context<'_>) -> UpdatePlan {
    let state = ctx.data_unchecked::<AppState>();
    let plan = super::super::super::update::planned();
    if state.current_config().update_check {
        state.update_check.read_and_maybe_refresh(
            plan.method.channel(),
            super::super::super::config_types::API_VERSION,
        );
    }
    UpdatePlan::from_plan(
        &plan,
        super::super::super::config_types::API_VERSION,
        &state.update_check.peek(),
    )
}

/// Every profile the file holds, as this schema describes them.
///
/// Shared by the listing, the lookup, `node` and the writes, so "what is there
/// now" is one shape whichever asked. A file that does not load has no
/// profiles here and says why on `config.yoloFile.error`, which is the one
/// place that status lives.
pub(crate) fn yolo_profiles() -> Vec<YoloProfile> {
    super::super::super::yolo::profiles()
        .iter()
        .map(|profile| YoloProfile::from_profile(profile))
        .collect()
}

/// The order the profiles are read in when nothing says otherwise: by name,
/// which is the key the file holds one profile per.
fn profiles_by_name() -> Vec<Term<YoloProfileOrderField>> {
    vec![Term {
        field: YoloProfileOrderField::Name,
        direction: OrderDirection::Asc,
    }]
}

/// The yolo profiles this machine has configured.
pub(crate) async fn yolo_profile_page(
    filter: Option<YoloProfileFilter>,
    order_by: Option<Vec<YoloProfileOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<YoloProfile>> {
    connection(
        yolo_profiles(),
        filter,
        terms(order_by, YoloProfileOrder::term, profiles_by_name),
        |profile: &YoloProfile| profile.name.clone(),
        Window {
            first,
            after,
            cap: PAGE_CAP,
            cap_name: "the yolo profile page cap",
        },
    )
    .await
}

/// One yolo profile, by the name `--yolo=<name>` spells.
pub(crate) fn yolo_profile(name: &str) -> Option<YoloProfile> {
    yolo_profiles()
        .into_iter()
        .find(|profile| profile.name == name)
}

/// The config as this schema describes it, with every secret left out.
///
/// Shared with the write side, so a config read and the answer to a config write
/// are the same shape rather than two that drifted.
pub(crate) fn config_of(
    config: &crate::config::Config,
    requests: &super::super::super::request_limits::RequestLimits,
    health: &crate::daemon::config_reload::ConfigHealth,
    admin_enabled: bool,
) -> Config {
    let redacted = super::super::super::config::redact(config, requests, health);
    let yolo = super::super::super::yolo::listing();
    // Before the struct below, which takes the config's own fields by value.
    let providers = providers_of(&redacted);
    Config {
        routing: RoutingConfig {
            default_provider: redacted.default_provider,
            provider_order: redacted.provider_order,
            override_model: redacted.override_model,
            fallback_model: redacted.fallback_model,
        },
        providers,
        gateways: redacted
            .gateways
            .iter()
            .map(|gateway| Gateway {
                name: gateway.name.clone(),
                kind: GatewayKind::from_wire(&gateway.kind),
                base_url: gateway.base_url.clone(),
                script: gateway.script.clone(),
                has_api_key: gateway.has_api_key,
                header_names: gateway.header_names.clone(),
                models: gateway.models.clone(),
                unknown_keys: gateway.extra_keys.clone(),
            })
            .collect(),
        allows_file_uploads: redacted.file_uploads,
        blueprint_paths: redacted
            .agent_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        mcp_server_count: count(redacted.mcp_server_count),
        server: ServerInfo {
            api_version: redacted.api_version,
            capabilities: redacted.capabilities,
            is_admin_enabled: admin_enabled,
            limits: ServeLimits {
                max_page_size: count(redacted.limits.max_limit),
                max_ids: count(redacted.limits.max_ids),
                max_file_bytes: BigInt(redacted.limits.max_file_bytes as i64),
                max_listing_entries: count(redacted.limits.max_listing_entries),
                max_search_scan: count(redacted.limits.max_search_scan),
                max_history_limit: count(redacted.limits.max_history_limit),
                max_concurrent_requests: BigInt(redacted.limits.max_concurrent_requests as i64),
                max_upload_bytes: BigInt(requests.max_upload_bytes as i64),
                request_timeout_secs: i32::try_from(requests.request_timeout_secs)
                    .unwrap_or(i32::MAX),
            },
        },
        health: ConfigHealth {
            error: redacted.config_error.map(|error| ConfigError {
                kind: ConfigErrorKind::from_wire(&error.kind),
                path: error.path,
                message: error.message,
                line: error.line.and_then(|line| i32::try_from(line).ok()),
                column: error.column.and_then(|col| i32::try_from(col).ok()),
                key: error.key,
                since: Timestamp(error.since),
                note: error.note,
            }),
            saved_at: redacted.config_mtime.map(Timestamp),
        },
        yolo_file: ConfigFileStatus {
            path: yolo.path,
            exists: yolo.exists,
            error: yolo.error,
        },
    }
}

/// Every provider this build knows, in a fixed order, as this machine has it
/// set up.
///
/// Fixed rather than "the ones that are configured": a settings screen draws a
/// row per provider whether or not it is on, and a list that grows and shrinks
/// under it is a list nobody can edit. The order is the ones that take a key
/// first, then the ones whose credential is a browser sign-in or nothing.
fn providers_of(
    redacted: &super::super::super::config_types::RedactedConfig,
) -> Vec<ProviderConfig> {
    /// One keyed provider's row: the id, the name to show, and whether a key
    /// is stored.
    fn keyed(id: &str, name: &str, has_key: bool, region: Option<String>) -> ProviderConfig {
        ProviderConfig {
            id: async_graphql::ID(id.to_string()),
            name: name.to_string(),
            auth: ProviderAuthKind::ApiKey,
            // A key is what puts a keyed provider in this install, so it is
            // also what turns it on. `clearKey` is how it is turned off.
            is_enabled: has_key,
            has_key,
            base_url: None,
            region,
            options: None,
        }
    }
    vec![
        keyed("anthropic", "Anthropic", redacted.has_anthropic_key, None),
        keyed("openai", "OpenAI", redacted.has_openai_key, None),
        keyed("google", "Google", redacted.has_google_key, None),
        keyed(
            "openrouter",
            "OpenRouter",
            redacted.has_openrouter_key,
            None,
        ),
        keyed(
            "bedrock",
            "Amazon Bedrock",
            redacted.has_bedrock_key,
            redacted.bedrock_region.clone(),
        ),
        keyed("xai", "xAI", redacted.has_xai_key, None),
        keyed("meta", "Meta", redacted.has_meta_key, None),
        ProviderConfig {
            id: async_graphql::ID("ollama".to_string()),
            name: "Ollama".to_string(),
            auth: ProviderAuthKind::None,
            is_enabled: redacted.ollama_enabled,
            has_key: false,
            base_url: redacted.ollama_base_url.clone(),
            region: None,
            options: None,
        },
        ProviderConfig {
            id: async_graphql::ID("codex".to_string()),
            name: "Codex".to_string(),
            auth: ProviderAuthKind::SignIn,
            is_enabled: redacted.codex_enabled,
            has_key: false,
            base_url: None,
            region: None,
            options: Some(ProviderOptions::Codex(CodexOptions {
                reasoning_effort: redacted
                    .codex_reasoning_effort
                    .as_deref()
                    .and_then(CodexReasoningEffort::from_wire),
                verbosity: redacted
                    .codex_verbosity
                    .as_deref()
                    .and_then(CodexVerbosity::from_wire),
                replays_reasoning: redacted.codex_replay_reasoning,
            })),
        },
        ProviderConfig {
            id: async_graphql::ID("grok".to_string()),
            name: "Grok".to_string(),
            auth: ProviderAuthKind::SignIn,
            is_enabled: redacted.grok_enabled,
            has_key: false,
            base_url: None,
            region: None,
            options: None,
        },
    ]
}

/// One diagnostics run as this schema describes it.
///
/// The offline checks: `isLive` is false, because nothing was dialled. The
/// live mutation calls [`live_doctor_report`] instead, and the two answer with
/// the same shape so a client renders one view.
pub(crate) fn doctor_report(checks: Vec<super::super::super::types::DoctorCheck>) -> DoctorReport {
    report_of(checks, false)
}

/// One diagnostics run that reached the network, as this schema describes it.
pub(crate) fn live_doctor_report(
    checks: Vec<super::super::super::types::DoctorCheck>,
) -> DoctorReport {
    report_of(checks, true)
}

/// The shape both reports take, differing only in whether anything was dialled.
fn report_of(checks: Vec<super::super::super::types::DoctorCheck>, is_live: bool) -> DoctorReport {
    DoctorReport {
        ok: checks.iter().all(|check| check.ok),
        is_live,
        checks: checks
            .into_iter()
            .map(|check| DoctorCheck {
                name: check.name,
                ok: check.ok,
                detail: check.detail,
            })
            .collect(),
    }
}

/// Narrow a count to the 32 bits GraphQL's `Int` carries.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
