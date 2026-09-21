//! The `config`, `doctor`, `mcpServers`, `yoloProfiles`, `mime`, `scripts`,
//! `directories`, `daemon`, `journal` and `update` fields: everything a
//! settings screen or a diagnostics view asks about the machine this server
//! runs on, rather than about a run.

use async_graphql::Context;
use leviath_runtime::control_socket::ControlResponse;

use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::BlueprintInput;
use super::super::scalars::{BigInt, Timestamp};
use super::super::types::machine::{
    Config, ConfigError, Directory, DoctorCheck, DoctorReport, Gateway, JournalHealth, McpServer,
    MimeRow, Script, ServeLimits, YoloHuman, YoloProfile, YoloProfiles, YoloWaiver,
};
use super::super::types::update::{DaemonStatus, UpdateInfo};

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

/// The MCP servers this machine has configured.
pub(crate) async fn mcp_servers(ctx: &Context<'_>) -> async_graphql::Result<Vec<McpServer>> {
    let state = ctx.data_unchecked::<AppState>();
    Ok(super::super::super::mcp::server_infos(state)
        .gql()?
        .into_iter()
        .map(McpServer::from_info)
        .collect())
}

/// The operator's mime registry, before any blueprint's own rows.
pub(crate) async fn mime(ctx: &Context<'_>) -> Vec<MimeRow> {
    let state = ctx.data_unchecked::<AppState>();
    super::super::super::blobs::mime_rows(state)
        .into_iter()
        .map(|row| MimeRow {
            mime_type: row.mime_type,
            source: row.source,
            family: row.family,
            is_text: row.text,
            extensions: row.extensions.unwrap_or_default(),
        })
        .collect()
}

/// The scripts this machine has registered.
pub(crate) async fn scripts(
    ctx: &Context<'_>,
    blueprint: Option<BlueprintInput>,
) -> async_graphql::Result<Vec<Script>> {
    let state = ctx.data_unchecked::<AppState>();
    let named = match blueprint {
        Some(input) => Some(input.installed(state).await.gql()?),
        None => None,
    };
    Ok(
        super::super::super::scripts::registered(state, named.as_deref())
            .gql()?
            .into_iter()
            .map(Script::from_item)
            .collect(),
    )
}

/// The directories under a path, for a file picker.
///
/// Confined to `--workdir-root` when the operator set one, which is also
/// why `parent` is null at that fence rather than leading above it.
pub(crate) async fn directories(
    ctx: &Context<'_>,
    path: Option<String>,
    hidden: bool,
) -> async_graphql::Result<Directory> {
    let state = ctx.data_unchecked::<AppState>();
    let listing = super::super::super::fs::dir_listing(state, path.as_deref(), hidden).gql()?;
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

/// Whether the daemon is still recording what its runs do.
///
/// Null when the daemon cannot be reached, because this is the daemon's own
/// reading and no other copy of it exists - `daemon.reachable` says whether
/// that is why. Everything else about a run is read from disk and keeps
/// working while the daemon is down; this does not.
///
/// Worth asking on any page that shows runs as healthy. A daemon whose
/// journal is refusing writes serves every field here exactly as it did
/// before, and a run whose journal record cannot be written is failed rather
/// than carried on.
pub(crate) async fn journal(ctx: &Context<'_>) -> Option<JournalHealth> {
    let state = ctx.data_unchecked::<AppState>();
    match state.control.list().await {
        Ok(ControlResponse::List { health, .. }) => Some(JournalHealth::of(&health.journal)),
        Ok(_) | Err(_) => None,
    }
}

/// What an update would do, and whether there is anything newer to get.
///
/// Planning never reaches the network. The "is there anything newer" half is
/// whatever the last check found, and asking starts another one for whoever
/// asks next rather than waiting on one here, so this is cheap enough for a
/// page to ask every time it opens.
pub(crate) async fn update(ctx: &Context<'_>) -> UpdateInfo {
    let state = ctx.data_unchecked::<AppState>();
    let plan = super::super::super::update::planned();
    if state.current_config().update_check {
        state.update_check.read_and_maybe_refresh(
            plan.method.channel(),
            super::super::super::config_types::API_VERSION,
        );
    }
    UpdateInfo::from_plan(
        &plan,
        super::super::super::config_types::API_VERSION,
        &state.update_check.peek(),
    )
}

/// What a profile's default does, in the word the config file uses.
pub(super) fn waiver_word(waiver: crate::yolo::rules::Waiver) -> YoloWaiver {
    match waiver {
        crate::yolo::rules::Waiver::Allow => YoloWaiver::Allow,
        crate::yolo::rules::Waiver::Ask => YoloWaiver::Ask,
    }
}

/// Whether a human-in-the-loop mechanism reaches a person, in the same words.
fn human_word(human: crate::yolo::rules::Human) -> YoloHuman {
    match human {
        crate::yolo::rules::Human::Ask => YoloHuman::Ask,
        crate::yolo::rules::Human::Auto => YoloHuman::Auto,
    }
}

/// The yolo profiles as this schema describes them.
///
/// Shared by the field and the write, so "what is there now" is one shape
/// whichever asked.
pub(crate) fn yolo_profiles() -> YoloProfiles {
    let listing = super::super::super::yolo::listing();
    YoloProfiles {
        path: listing.path,
        exists: listing.exists,
        error: listing.error,
        profiles: listing
            .profiles
            .into_iter()
            .map(|profile| YoloProfile {
                id: super::super::node::yolo_profile_id(&profile.name),
                name: profile.name,
                default: waiver_word(profile.default),
                questions: human_word(profile.questions),
                checkpoints: human_word(profile.checkpoints),
                gate: human_word(profile.gate),
                tool_rules: profile.tool_rules.iter().map(|n| count(*n)).collect(),
                shell_rules: profile.shell_rules.iter().map(|n| count(*n)).collect(),
            })
            .collect(),
    }
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
    let mut configured = Vec::new();
    for (name, present) in [
        ("anthropic", redacted.has_anthropic_key),
        ("openai", redacted.has_openai_key),
        ("google", redacted.has_google_key),
        ("openrouter", redacted.has_openrouter_key),
        ("bedrock", redacted.has_bedrock_key),
        ("xai", redacted.has_xai_key),
        ("meta", redacted.has_meta_key),
    ] {
        if present {
            configured.push(name.to_string());
        }
    }
    Config {
        default_provider: redacted.default_provider,
        provider_order: redacted.provider_order,
        override_model: redacted.override_model,
        fallback_model: redacted.fallback_model,
        configured_providers: configured,
        gateways: redacted
            .gateways
            .iter()
            .map(|gateway| Gateway {
                name: gateway.name.clone(),
                base_url: gateway.base_url.clone(),
                has_api_key: gateway.has_api_key,
                kind: gateway.kind.clone(),
            })
            .collect(),
        agent_paths: redacted
            .agent_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        mcp_server_count: count(redacted.mcp_server_count),
        api_version: redacted.api_version,
        capabilities: redacted.capabilities,
        admin_enabled,
        limits: ServeLimits {
            max_page_size: count(redacted.limits.max_limit),
            max_ids: count(redacted.limits.max_ids),
            max_file_bytes: BigInt(redacted.limits.max_file_bytes as i64),
            max_listing_entries: count(redacted.limits.max_listing_entries),
            max_search_scan: count(redacted.limits.max_search_scan),
            max_history_limit: count(redacted.limits.max_history_limit),
            max_concurrent_requests: BigInt(redacted.limits.max_concurrent_requests as i64),
            max_upload_bytes: BigInt(requests.max_upload_bytes as i64),
            request_timeout_secs: i32::try_from(requests.request_timeout_secs).unwrap_or(i32::MAX),
        },
        config_error: redacted.config_error.map(|error| ConfigError {
            kind: error.kind,
            path: error.path,
            message: error.message,
            line: error.line.and_then(|line| i32::try_from(line).ok()),
            column: error.column.and_then(|col| i32::try_from(col).ok()),
            key: error.key,
            since: Timestamp(error.since),
            note: error.note,
        }),
        config_mtime: redacted.config_mtime.map(Timestamp),
    }
}

/// One diagnostics run as this schema describes it.
///
/// Shared by the offline field and the live mutation: they run different checks
/// and answer with the same shape, which is what lets a client render one view.
pub(crate) fn doctor_report(checks: Vec<super::super::super::types::DoctorCheck>) -> DoctorReport {
    DoctorReport {
        ok: checks.iter().all(|check| check.ok),
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
