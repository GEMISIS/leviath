//! Agent CRUD endpoints: spawn, list, get, kill, children, context, logs, result.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use super::blueprints::discover_blueprints;
use super::types::*;
use crate::runstate::{self, ContextSnapshot, RunMeta};

/// `POST /api/agents`: spawn an agent into the shared-world daemon.
///
/// Resolves the blueprint's manifest path, mints a run id, and asks the daemon
/// (over the control socket) to create the agent; the daemon loads the blueprint,
/// resolves tools/model, and persists the run so the read endpoints observe it.
///
/// `yolo` / `allow` / `max_depth` from the request are forwarded through
/// [`SpawnArgs`] to the daemon's tool-policy resolution.
/// The output shape this request asks for, or `None` when it asks for nothing
/// (leaving whatever the blueprint declares).
///
/// `output_format` is carried through as an opaque label and never checked
/// against a known set, which is what lets a client ask for a2ui, a media type,
/// or a house format without any server-side support. `output_schema` is the
/// one field with meaning here, and only because the runtime will check it.
fn output_request(body: &SpawnAgentReq) -> Option<leviath_core::output::OutputSpec> {
    if body.output_format.is_none()
        && body.output_instructions.is_none()
        && body.output_schema.is_none()
    {
        return None;
    }
    Some(leviath_core::output::OutputSpec {
        format: body.output_format.clone(),
        instructions: body.output_instructions.clone(),
        example: None,
        schema: body.output_schema.clone(),
        validator: None,
    })
}

pub(super) async fn spawn_agent(
    State(state): State<AppState>,
    Json(body): Json<SpawnAgentReq>,
) -> Result<Json<SpawnAgentResp>, (StatusCode, Json<ErrorResponse>)> {
    let blueprints = discover_blueprints(&state.current_config());
    let bp_info = blueprints
        .iter()
        .find(|b| b.name == body.blueprint)
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                format!("Blueprint '{}' not found", body.blueprint),
            )
        })?;
    let manifest_path = PathBuf::from(&bp_info.path).join("agent.leviath");

    let workdir = body.workdir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    // A caller-supplied workdir was accepted verbatim, so `{"workdir": "/"}`
    // pointed a tool-executing agent at the whole filesystem. `--workdir-root`
    // is the operator's answer to "where is this API allowed to work".
    state
        .limits
        .check_workdir(std::path::Path::new(&workdir))
        .map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    // Likewise `{"yolo": true}` waived every approval prompt for an agent
    // running on the host, from a request - as did `{"allow": ["*"]}`, which
    // reaches the same wildcard override by another name. `--no-remote-yolo`
    // refuses both.
    state
        .limits
        .check_launch_overrides(body.yolo, &body.allow)
        .map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    // And a completion webhook is a request the daemon makes on the caller's
    // behalf, so it goes through the same SSRF policy as any model-supplied URL.
    if let Some(callback) = body.callback_url.as_deref() {
        state
            .limits
            .check_callback_url(callback)
            .map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    }
    let run_id = runstate::new_run_id(&body.blueprint);
    let args = SpawnArgs {
        run_id,
        blueprint_path: manifest_path.to_string_lossy().to_string(),
        task: body.task.clone(),
        regions: body.regions.clone(),
        model: body.model.clone(),
        workdir,
        metadata: body.metadata.clone(),
        callback_url: body.callback_url.clone(),
        callback_secret: body.callback_secret.clone(),
        yolo: body.yolo,
        no_seed_commands: body.no_seed_commands,
        allow: body.allow.clone(),
        output: output_request(&body),
        max_depth: body.max_depth,
        // Serve spawns are top-level runs.
        parent_run_id: None,
    };

    match state.control.spawn(args).await {
        Ok(ControlResponse::Spawned { run_id }) => {
            // No `AgentSpawned` from here. The daemon's change-detection pass
            // emits one for every run the world gains, however it was
            // launched, and this route used to emit a second for its own - so
            // a subscriber saw the run appear twice, and only ever for the
            // runs that came in through HTTP.
            tracing::info!(run_id = %run_id, blueprint = %body.blueprint, "spawned agent via API");
            Ok(Json(SpawnAgentResp {
                agent_id: run_id.clone(),
                run_id,
            }))
        }
        Ok(ControlResponse::Error { message }) => Err(err(
            StatusCode::BAD_REQUEST,
            format!("Failed to spawn agent: {message}"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(daemon_error(e)),
    }
}

pub(super) async fn list_agents(Query(query): Query<ListAgentsQuery>) -> Json<Vec<RunMeta>> {
    let mut runs = runstate::list_runs();

    if let Some(ref status_filter) = query.status {
        let filters: Vec<&str> = status_filter.split(',').collect();
        runs.retain(|r| filters.iter().any(|f| status_matches(&r.status, f)));
    }

    // `.redacted()`: `RunMeta` carries the webhook signing secret, and this
    // handler serializes it whole.
    Json(runs.iter().map(RunMeta::redacted).collect())
}

pub(super) async fn get_agent(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunMeta>, (StatusCode, Json<ErrorResponse>)> {
    runstate::read_meta(&id)
        .map(|m| Json(m.redacted()))
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Agent run '{}' not found", id),
                }),
            )
        })
}

pub(super) async fn agent_children(AxumPath(id): AxumPath<String>) -> Json<Vec<RunMeta>> {
    let runs = runstate::list_runs();
    let children: Vec<RunMeta> = runs
        .into_iter()
        .filter(|r| r.parent_run_id.as_deref() == Some(&id))
        .map(|r| r.redacted())
        .collect();
    Json(children)
}

pub(super) async fn agent_context(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ContextSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    runstate::read_context_snapshot(&id)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("No context snapshot for run '{}'", id),
                }),
            )
        })
}

/// Default number of history points returned when the client does not ask.
pub(super) const HISTORY_DEFAULT_LIMIT: usize = 50;
/// Largest history page. Lower than the run listing's cap because each item
/// carries a whole context window rather than one struct of scalars.
pub(super) const HISTORY_MAX_LIMIT: usize = 100;

/// `GET /api/agents/{id}/context/history`: the run's context window over time.
///
/// This returned **every** recorded point, each carrying a full
/// `ContextSnapshot` with untruncated region text - comfortably the largest
/// response in the API, with no window and no cap, on a journal that grows for
/// as long as the run does.
///
/// Now paged, through the same envelope the run listing uses. The cursor is the
/// point index: the journal is append-only, so an index is stable once written
/// and new points only ever arrive at the end - the strongest keyset of any
/// route here. `order=asc` stays the default, which is both chronological and
/// what the previous unpaged response gave.
///
/// The replay itself goes through `visit_points`, so points outside the window
/// are folded but never materialized. Materializing means deep-copying an
/// entire context window per point, and this endpoint's whole problem was doing
/// that for the full history on every request.
pub(super) async fn agent_context_history(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Page<leviath_core::run_archive::RunPoint>>, (StatusCode, Json<ErrorResponse>)> {
    use std::ops::ControlFlow;

    let ascending = match query.order.as_deref() {
        None | Some("asc") => true,
        Some("desc") => false,
        Some(other) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("Unknown order '{other}': expected asc or desc"),
            ));
        }
    };
    let limit = match query.limit {
        None => HISTORY_DEFAULT_LIMIT,
        Some(0) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "`limit` must be at least 1; omit it for the default".to_string(),
            ));
        }
        Some(n) => n.min(HISTORY_MAX_LIMIT),
    };

    let digest = super::cursor::filter_digest(&[&id]);
    let order_name = if ascending { "asc" } else { "desc" };
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => Some(
            super::cursor::decode(raw, "index", order_name, &digest)
                .map_err(|e| err(StatusCode::BAD_REQUEST, e.message()))?,
        ),
    };

    // One streamed pass to count, so `total` is honest and a descending window
    // knows where to start. Counting folds the deltas but materializes nothing
    // - and streaming means the journal is never parsed whole into memory:
    // `read_run_archive` materialized every record (tens of MB for a mature
    // run, 2-4x that as parsed structs) per request, which was this API's
    // single largest transient allocation and the reason a page refresh
    // stair-stepped the server's RSS.
    let mut total = 0usize;
    if runstate::visit_run_archive(&id, &mut |_| {
        total += 1;
        ControlFlow::Continue(())
    })
    .is_none()
    {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("No context history for run '{id}'"),
        ));
    }
    if total == 0 && query.cursor.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("No context history for run '{id}'"),
        ));
    }

    // Which indices this page wants, given the direction and where the cursor
    // left off. Computed up front so the replay can skip everything else.
    // This route only ever mints an integer key, so anything else means a
    // cursor that did not come from here - and the `_` arm is what the common
    // "no cursor at all" case takes too.
    let after = match cursor.as_ref().map(|c| &c.key) {
        Some(super::cursor::CursorKey::Int(i)) => usize::try_from(*i).ok(),
        _ => None,
    };
    let wanted: Vec<usize> = if ascending {
        let start = after.map(|i| i + 1).unwrap_or(0);
        (start..total).take(limit + 1).collect()
    } else {
        let start = after
            .map(|i| i.saturating_sub(1))
            .unwrap_or_else(|| total.saturating_sub(1));
        (0..=start).rev().take(limit + 1).collect()
    };
    let stop_at = wanted.iter().copied().max();

    let mut collected: Vec<(usize, leviath_core::run_archive::RunPoint)> = Vec::new();
    runstate::visit_run_archive(&id, &mut |point| {
        if wanted.contains(&point.index) {
            collected.push((
                point.index,
                leviath_core::run_archive::RunPoint {
                    // Redacted for the same reason `runstate::context_history`
                    // redacts: the journal stores RunMeta whole, secret and all.
                    meta: point.meta.redacted(),
                    context: point.context.clone(),
                    at: point.at,
                },
            ));
        }
        match stop_at {
            Some(last) if point.index >= last => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    });

    if !ascending {
        collected.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    }

    let has_more = collected.len() > limit;
    collected.truncate(limit);
    let next_cursor = has_more
        .then(|| collected.last())
        .flatten()
        .map(|(index, _)| {
            super::cursor::encode(
                "index",
                order_name,
                &digest,
                super::cursor::CursorKey::Int(*index as i64),
                "",
            )
        });

    let items = collected.into_iter().map(|(_, point)| point).collect();
    Ok(Json(Page::new(items, next_cursor, Some(total), now_secs())))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `GET /api/agents/{id}/logs`: what a run has written, by stage.
///
/// This read `<run_dir>/output.log`, a path nothing in the codebase writes, so
/// it returned an empty string for every run that has ever existed - a run's
/// real output lives under `stages/<idx>/`. `?stage=` and `?stream=` select
/// which log; both default to what a caller tailing a live run wants (the
/// current stage's readable output). Response stays a bare string, so an
/// existing client sees only that it finally has content.
pub(super) async fn agent_logs(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let run_dir = runstate::run_dir(&id);
    if !run_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        ));
    }

    let selector = query
        .selector()
        .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;
    let stream = query
        .log_stream()
        .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;

    // Clamped like every other limit in this API: `tail` is client-controlled
    // and `stage=all` multiplies it by the stage count, so an unclamped value
    // was an arbitrary-size allocation on request.
    let max_bytes = query.tail.unwrap_or(32_768).min(LOGS_MAX_TAIL_BYTES);
    Ok(runstate::tail_run_logs(&id, selector, stream, max_bytes))
}

/// Largest per-stage byte window `GET /api/agents/{id}/logs?tail=` honors:
/// 1 MiB, matching the file endpoint's cap.
pub(super) const LOGS_MAX_TAIL_BYTES: u64 = 1024 * 1024;

/// How much of a file `GET /api/agents/{id}/files` returns: 1 MiB, enough for
/// any report a browser would render, small enough to hand out in one JSON body.
pub(super) const MAX_FILE_READ_BYTES: u64 = 1024 * 1024;

/// `GET /api/agents/{id}/files?path=<path>`: read a file the run wrote, so the
/// browser can render an agent's report without shell access to the host.
///
/// `path` may be relative (resolved against the run's workdir) or absolute;
/// either way the *resolved* path must still land inside the workdir, under the
/// same symlink-aware containment the file tools use
/// ([`leviath_core::resolves_within`]) - so this endpoint can read exactly what
/// the run was already allowed to write, and nothing else. Reads are capped at
/// [`MAX_FILE_READ_BYTES`]; a larger file comes back truncated and says so.
/// Purely a filesystem read: it works with the daemon down, like the other
/// read endpoints.
pub(super) async fn agent_file(
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileOrListing>, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Agent run '{id}' not found")))?;

    let source = query
        .file_source()
        .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;

    // No path means "what is there", which is a listing rather than a read.
    let Some(ref requested_path) = query.path else {
        return list_run_files(&meta, source, None, query.hidden).map(Json);
    };

    let workdir = PathBuf::from(&meta.workdir);
    let requested = PathBuf::from(requested_path);
    let resolved = match requested.is_absolute() {
        true => requested,
        false => workdir.join(&requested),
    };
    if !leviath_core::resolves_within(&resolved, &workdir) {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!("path '{requested_path}' is outside the run's working directory"),
        ));
    }

    let size = match std::fs::metadata(&resolved) {
        // A directory used to be a 400. It is the natural way to ask "what is
        // in here", and the folder picker already answers that shape, so it
        // lists instead. Nothing can have depended on the old error.
        Ok(m) if m.is_dir() => {
            return list_run_files(&meta, FileSource::Workdir, Some(&resolved), query.hidden)
                .map(Json);
        }
        Ok(m) => m.len(),
        Err(_) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                format!("file '{requested_path}' not found"),
            ));
        }
    };

    // Where in the file to start. A run's artifact can be far larger than one
    // response - a dataset is the whole point of some agents - so a caller pages
    // through with `offset` rather than being stuck with the first megabyte.
    let offset = query.offset.unwrap_or(0);
    if offset > size {
        return Err(err(
            StatusCode::RANGE_NOT_SATISFIABLE,
            format!("offset {offset} is past the end of '{requested_path}' ({size} bytes)"),
        ));
    }

    let mut bytes = Vec::new();
    if let Err(e) = std::fs::File::open(&resolved)
        // Chained rather than `?`: the offset is already bounded by the file's
        // size above, so a seek into it has no reachable failure of its own.
        .and_then(|mut f| std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset)).map(|_| f))
        .map(|f| std::io::Read::take(f, MAX_FILE_READ_BYTES))
        .and_then(|mut f| std::io::Read::read_to_end(&mut f, &mut bytes))
    {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("could not read '{requested_path}': {e}"),
        ));
    }
    let read_len = bytes.len();
    let next_offset = offset + read_len as u64;
    let truncated = next_offset < size;

    // A byte offset can land mid-character at either end. Trimming a partial
    // character off the front keeps the window aligned so the *next* page starts
    // on a boundary, which is what makes concatenating pages give back the file.
    let leading_partial = bytes
        .iter()
        .take(4)
        .position(|b| !is_utf8_continuation(*b))
        .filter(|_| offset > 0)
        .unwrap_or(0);
    let bytes = bytes.split_off(leading_partial);
    let read_len = read_len - leading_partial;

    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        // The cap can land mid-character in a file that is otherwise valid
        // UTF-8. That is the cap's doing, not the file's: drop the split
        // character's leading bytes rather than calling a text file binary.
        // (`valid_up_to` is at most 3 bytes short of the end when the only
        // problem is the cut.)
        Err(e) if truncated && e.utf8_error().valid_up_to() + 4 > read_len => {
            let valid = e.utf8_error().valid_up_to();
            let mut prefix = e.into_bytes();
            prefix.truncate(valid);
            String::from_utf8_lossy(&prefix).into_owned()
        }
        Err(_) => {
            return Err(err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("'{requested_path}' is not a text file"),
            ));
        }
    };

    Ok(Json(FileOrListing::File(FileContentResp {
        path: resolved.to_string_lossy().into_owned(),
        size,
        // Where this window actually started and ended, so a caller can ask for
        // the next one without guessing what the UTF-8 trim did.
        offset: offset + leading_partial as u64,
        next_offset: match truncated {
            true => Some(offset + leading_partial as u64 + content.len() as u64),
            false => None,
        },
        content,
        truncated,
    })))
}

/// Whether `b` is a UTF-8 continuation byte (`10xxxxxx`), i.e. the middle of a
/// character rather than the start of one.
fn is_utf8_continuation(b: u8) -> bool {
    (b & 0b1100_0000) == 0b1000_0000
}

/// Most entries one directory listing returns.
///
/// A single `node_modules/.pnpm` really does hold six figures of entries, and
/// the response is built in memory.
pub(super) const MAX_LISTING_ENTRIES: usize = 1000;

/// List what a run touched, or what is in its working directory.
///
/// Two genuinely different questions, and neither substitutes for the other:
///
/// - [`FileSource::Modified`] is the run's own record of what it changed. Free -
///   it is already in `meta.json` - but capped at record time, so it is a claim
///   about the run rather than about the disk.
/// - [`FileSource::Workdir`] is what is actually there now, read **one directory
///   level per request**. That bound is the answer to "a repo with
///   node_modules": the client walks the tree itself, exactly as the existing
///   folder picker does, instead of one request trying to enumerate everything.
fn list_run_files(
    meta: &RunMeta,
    source: FileSource,
    dir: Option<&std::path::Path>,
    hidden: bool,
) -> Result<FileOrListing, (StatusCode, Json<ErrorResponse>)> {
    let workdir = PathBuf::from(&meta.workdir);
    let listing = match source {
        FileSource::Modified => modified_listing(meta, &workdir),
        FileSource::Workdir => workdir_listing(meta, &workdir, dir, hidden)?,
    };
    Ok(FileOrListing::Listing(Box::new(listing)))
}

/// The paths the run recorded modifying, stat-ed against the workdir.
fn modified_listing(meta: &RunMeta, workdir: &std::path::Path) -> RunFileListing {
    let entries = meta
        .flags
        .modified_files
        .iter()
        .map(|rel| {
            let resolved = if std::path::Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                workdir.join(rel)
            };
            let stat = std::fs::metadata(&resolved).ok();
            RunFileEntry {
                name: std::path::Path::new(rel)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| rel.clone()),
                path: rel.clone(),
                is_dir: stat.as_ref().is_some_and(|m| m.is_dir()),
                size: stat.as_ref().map(|m| m.len()),
                // A recorded path can name a file since deleted, or - for a
                // tool given an absolute path - one outside the workdir.
                // Reported rather than filtered away, so the list stays a
                // faithful account of what the run did.
                exists: stat.is_some(),
                outside_workdir: !leviath_core::resolves_within(&resolved, workdir),
            }
        })
        .collect();

    RunFileListing {
        kind: "listing",
        source: "modified",
        path: meta.workdir.clone(),
        parent: None,
        workdir: meta.workdir.clone(),
        entries,
        truncated: false,
        // Both of the ways this list misleads, made visible in the response
        // rather than left in the documentation. See the field docs.
        modified_files_truncated: meta.flags.modified_files.len()
            >= leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES,
        modifying_tool_calls: meta.flags.modified_file_count,
    }
}

/// One directory level of the run's working directory.
fn workdir_listing(
    meta: &RunMeta,
    workdir: &std::path::Path,
    dir: Option<&std::path::Path>,
    hidden: bool,
) -> Result<RunFileListing, (StatusCode, Json<ErrorResponse>)> {
    let target = dir
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf());
    if !target.is_dir() {
        // Distinguished from an ordinary 404 because a lost workspace is a
        // known run outcome (`flags.workspace_lost`), and an empty listing
        // would read as "this run touched nothing".
        return Err(err(
            StatusCode::NOT_FOUND,
            format!(
                "the run's working directory '{}' no longer exists",
                target.display()
            ),
        ));
    }

    let mut entries: Vec<RunFileEntry> = Vec::new();
    let mut truncated = false;
    for child in std::fs::read_dir(&target).into_iter().flatten().flatten() {
        if entries.len() >= MAX_LISTING_ENTRIES {
            truncated = true;
            break;
        }
        let name = child.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        let path = child.path();
        // Per entry, not just for the directory asked for: a symlinked child
        // can point outside the fence. The folder picker already does this.
        if !leviath_core::resolves_within(&path, workdir) {
            continue;
        }
        let stat = child.metadata().ok();
        entries.push(RunFileEntry {
            path: path
                .strip_prefix(workdir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
            name,
            is_dir: stat.as_ref().is_some_and(|m| m.is_dir()),
            size: stat.as_ref().map(|m| m.len()),
            exists: true,
            outside_workdir: false,
        });
    }
    // Directories first, then by name - the grouping a file tree wants, done
    // once here rather than in every client.
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

    Ok(RunFileListing {
        kind: "listing",
        source: "workdir",
        path: target.to_string_lossy().into_owned(),
        parent: (target != workdir)
            .then(|| target.parent().map(|p| p.to_string_lossy().into_owned()))
            .flatten(),
        workdir: meta.workdir.clone(),
        entries,
        truncated,
        modified_files_truncated: meta.flags.modified_files.len()
            >= leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES,
        modifying_tool_calls: meta.flags.modified_file_count,
    })
}

pub(super) async fn agent_result(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<AgentResultResp>, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        )
    })?;

    // The last stage's output, through the same reader `agent_logs` uses -
    // including its fallback for a run with no stages recorded.
    let output = runstate::tail_run_logs(
        &id,
        runstate::StageSelector::Current,
        runstate::LogStream::Output,
        65_536,
    );

    Ok(Json(AgentResultResp {
        run_id: meta.run_id,
        // The word every other route uses. This one rendered the status
        // through `Display` - `WaitingInput`, `CompleteInteractive` - which is
        // the spelling for a person reading a terminal, not for a client
        // matching on it.
        status: meta.status.wire().to_string(),
        output,
        // The answer, when the agent gave one. `output` above is the last
        // stage's log tail, which is what this endpoint served before an agent
        // had any way to say "here is my result".
        //
        // Read from the sidecar rather than `meta`, which carries only the
        // descriptor: this endpoint is the one place that wants the whole thing,
        // which is exactly why the bytes are not in the file every listing
        // parses.
        final_output: runstate::read_final_output(&id).map(Into::into),
        error: meta.error,
        prompt_tokens: meta.prompt_tokens,
        completion_tokens: meta.completion_tokens,
    }))
}

/// `GET /api/agents/{id}/stages`: the run's per-stage ledger.
///
/// Read-only, and read through the same `stages.json` reader `lev stages`
/// uses, so the CLI and the API cannot disagree about a run.
///
/// A missing or unreadable index is an empty list rather than a 404: the run
/// exists - `read_meta` would have failed otherwise - and a run that has not
/// reached its first stage boundary legitimately has no records yet. Reporting
/// that as "no such run" would send a client back to re-ask a question that has
/// already been answered.
pub(super) async fn agent_stages(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunStagesResp>, (StatusCode, Json<ErrorResponse>)> {
    let meta = runstate::read_meta(&id).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent run '{}' not found", id),
            }),
        )
    })?;

    Ok(Json(RunStagesResp {
        stages: runstate::read_stages_index(&id),
        run_id: meta.run_id,
    }))
}

/// `DELETE /api/agents/{id}`: cancel a run in the shared-world daemon. The
/// daemon cancels the agent (cascading to its sub-agents in the one world) and
/// persists the terminal status.
pub(super) async fn kill_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Cancel { run_id: id.clone() })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::NO_CONTENT),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' not found"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(daemon_error(e)),
    }
}

/// `POST /api/agents/{id}/pause`: park a run. The daemon refuses when the run
/// does not exist or is not pausable (waiting on input, or finished), which
/// both surface as 404 - the daemon's reply does not distinguish them.
pub(super) async fn pause_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Pause { run_id: id.clone() })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::NO_CONTENT),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' not found or not pausable"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(daemon_error(e)),
    }
}

/// `POST /api/agents/{id}/resume`: un-pause a run.
pub(super) async fn resume_agent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    match state
        .control
        .request(&ControlRequest::Resume { run_id: id.clone() })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => Ok(StatusCode::NO_CONTENT),
        Ok(ControlResponse::Ok { ok: false }) => Err(err(
            StatusCode::NOT_FOUND,
            format!("Agent run '{id}' not found or not paused"),
        )),
        Ok(other) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected daemon response: {other:?}"),
        )),
        Err(e) => Err(daemon_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::commands::serve::testutil::fake_daemon;
    use crate::config::Config;
    use crate::runstate::{RunMeta, RunStatus, create_run};
    use leviath_runtime::control_socket::ControlClient;

    /// A control client at an address with no daemon (read endpoints don't use it).
    fn no_daemon() -> ControlClient {
        ControlClient::new(leviath_runtime::control_socket::control_id(
            std::path::Path::new("/no/such/daemon"),
        ))
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: no_daemon(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    /// The refusals as the handler returns them: a 403 before the request ever
    /// reaches the daemon, so a token holder cannot point an agent at the
    /// filesystem root or waive its approval prompts.
    #[tokio::test]
    async fn spawn_refuses_an_out_of_root_workdir_and_remote_yolo() {
        let root = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        let bp = agents.path().join("probe");
        std::fs::create_dir(&bp).unwrap();
        std::fs::write(
            bp.join("agent.leviath"),
            "[agent]\nname = \"probe\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nsystem_prompt = \"p\"\n",
        )
        .unwrap();

        let (tx, _) = broadcast::channel(64);
        let state = AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                agent_paths: vec![agents.path().to_path_buf()],
                ..Default::default()
            }),
            event_tx: tx,
            control: no_daemon(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Arc::new(ServeLimits {
                allow_local_network: false,
                workdir_root: Some(root.path().to_path_buf()),
                no_remote_yolo: true,
            }),
        };

        // Outside `--workdir-root`.
        let err = spawn_agent(
            State(state.clone()),
            Json(SpawnAgentReq {
                blueprint: "probe".to_string(),
                task: "t".to_string(),
                workdir: Some("/".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a workdir outside the root must be refused");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(
            err.1.0.error.contains("--workdir-root"),
            "{}",
            err.1.0.error
        );

        // Inside the root, but pointing the completion webhook at the cloud
        // metadata service - a request the daemon would make on the caller's
        // behalf, from inside the trust boundary.
        let err = spawn_agent(
            State(state.clone()),
            Json(SpawnAgentReq {
                blueprint: "probe".to_string(),
                task: "t".to_string(),
                workdir: Some(root.path().to_string_lossy().to_string()),
                callback_url: Some("http://169.254.169.254/latest/meta-data/".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("a webhook aimed at link-local must be refused");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(err.1.0.error.contains("callback_url"), "{}", err.1.0.error);

        // Inside the root, but asking for yolo - spelled both ways. `allow`
        // reaches the same wildcard override `yolo` writes, so guarding only
        // the `yolo` field left the refusal bypassable by asking differently.
        for (label, req) in [
            (
                "yolo",
                SpawnAgentReq {
                    yolo: true,
                    ..Default::default()
                },
            ),
            (
                "a wildcard allow",
                SpawnAgentReq {
                    allow: vec!["*".to_string()],
                    ..Default::default()
                },
            ),
            (
                "a named allow",
                SpawnAgentReq {
                    allow: vec!["shell".to_string()],
                    ..Default::default()
                },
            ),
        ] {
            let err = spawn_agent(
                State(state.clone()),
                Json(SpawnAgentReq {
                    blueprint: "probe".to_string(),
                    task: "t".to_string(),
                    workdir: Some(root.path().to_string_lossy().to_string()),
                    ..req
                }),
            )
            .await
            .expect_err("a waived approval must be refused under --no-remote-yolo");
            assert_eq!(err.0, StatusCode::FORBIDDEN, "{label}");
            assert!(
                err.1.0.error.contains("no-remote-yolo"),
                "{label}: {}",
                err.1.0.error
            );
        }
    }

    /// The refusal is `--no-remote-yolo`'s alone. A server that did not ask for
    /// it still honours both fields, or the flag would be doing something the
    /// operator never requested.
    #[test]
    fn launch_overrides_are_only_refused_under_no_remote_yolo() {
        let permissive = ServeLimits {
            allow_local_network: false,
            workdir_root: None,
            no_remote_yolo: false,
        };
        assert!(
            permissive
                .check_launch_overrides(true, &["*".to_string()])
                .is_ok()
        );

        let hardened = ServeLimits {
            no_remote_yolo: true,
            ..permissive
        };
        assert!(hardened.check_launch_overrides(false, &[]).is_ok());
        for (yolo, allow) in [
            (true, vec![]),
            (false, vec!["*".to_string()]),
            (false, vec!["read_file".to_string()]),
            (true, vec!["shell".to_string()]),
        ] {
            assert!(
                hardened.check_launch_overrides(yolo, &allow).is_err(),
                "yolo={yolo} allow={allow:?}"
            );
        }
    }

    /// `--workdir-root` bounds where an API caller may point an agent. Without
    /// it, `{"workdir": "/"}` handed a tool-executing agent the whole
    /// filesystem.
    #[test]
    fn workdir_root_bounds_the_requested_workdir() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("project");
        std::fs::create_dir(&inside).unwrap();
        let limits = ServeLimits {
            workdir_root: Some(root.path().to_path_buf()),
            ..Default::default()
        };

        assert!(limits.check_workdir(&inside).is_ok());
        assert!(limits.check_workdir(root.path()).is_ok());
        let err = limits.check_workdir(std::path::Path::new("/")).unwrap_err();
        assert!(err.contains("--workdir-root"), "{err}");
    }

    /// A completion webhook is a request the daemon makes on the caller's
    /// behalf, from inside the trust boundary and with retries. Unchecked,
    /// `"callback_url": "http://169.254.169.254/…"` turned the API into a
    /// repeatable request primitive against cloud metadata and the local
    /// network.
    #[test]
    fn a_callback_url_goes_through_the_same_ssrf_policy_as_any_other() {
        let limits = ServeLimits::default();
        for blocked in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9200/_shutdown",
            "http://192.168.1.1/admin",
            "file:///etc/passwd",
        ] {
            let err = limits
                .check_callback_url(blocked)
                .expect_err("{blocked} must be refused");
            assert!(err.contains("callback_url"), "{err}");
        }
        assert!(limits.check_callback_url("not a url").is_err());
    }

    /// The opt-out an operator can take deliberately.
    #[test]
    fn allow_local_network_lets_a_webhook_reach_this_host() {
        let limits = ServeLimits {
            allow_local_network: true,
            ..Default::default()
        };
        // The same URL the default refuses. That this flips on the setting --
        // rather than everything being refused either way - is what shows the
        // check is reading the address and not just saying no.
        let url = "http://127.0.0.1:9000/hook";
        assert!(limits.check_callback_url(url).is_ok());
        assert!(ServeLimits::default().check_callback_url(url).is_err());
    }

    /// The containment is symlink-aware, so a link planted under the root cannot
    /// be used to walk out of it.
    #[cfg(unix)]
    #[test]
    fn workdir_root_is_not_fooled_by_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let limits = ServeLimits {
            workdir_root: Some(root.path().to_path_buf()),
            ..Default::default()
        };
        assert!(limits.check_workdir(&root.path().join("escape")).is_err());
    }

    /// With no `--workdir-root`, behavior is unchanged - the flag is opt-in for
    /// operators, not a new hard requirement on every deployment.
    #[test]
    fn no_workdir_root_permits_anything() {
        let limits = ServeLimits::default();
        assert!(limits.check_workdir(std::path::Path::new("/")).is_ok());
    }

    fn unique_run_id(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("test-{}-{}-{}", prefix, std::process::id(), id)
    }

    fn make_run(id: &str) -> RunMeta {
        RunMeta::new(
            id.to_string(),
            "test-agent".to_string(),
            "/path/to/agent".to_string(),
            "do something".to_string(),
            None,
            "/tmp".to_string(),
            1,
        )
    }

    /// A `stages.json` entry, so a log test can say which stages exist.
    fn stage_rec(index: usize, name: &str) -> leviath_core::run_meta::StageRecord {
        leviath_core::run_meta::StageRecord {
            active: Default::default(),
            name: name.to_string(),
            index,
            status: leviath_core::run_meta::StageRunStatus::Complete,
            entered: true,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            region_tokens: Default::default(),
            first_call_prompt_tokens: None,
            runaway_warned: false,
            started_at: None,
            ended_at: None,
        }
    }

    /// Call `GET /api/agents/{id}/logs{query}` and return the body as text.
    /// The endpoint's whole bug was that its body was always empty, so a log
    /// test that does not read the body proves nothing.
    async fn logs_body(run_id: &str, query: &str) -> String {
        let app = Router::new()
            .route("/api/agents/{id}/logs", get(agent_logs))
            .with_state(test_state());
        let req = Request::builder()
            .uri(format!("/api/agents/{run_id}/logs{query}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn test_state_with_agent_paths(paths: Vec<PathBuf>, control: ControlClient) -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                agent_paths: paths,
                ..Default::default()
            }),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    fn write_test_blueprint(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "A spawnable test blueprint"

[stages.plan]
system_prompt = "Plan the work"
"#
            ),
        )
        .unwrap();
    }

    // ─── spawn_agent ──────────────────────────────────────────────────────────

    /// A router over `POST /api/agents` backed by `control`, plus a temp agents
    /// dir holding one discoverable blueprint named "spawnable".
    fn spawn_app(control: ControlClient) -> (Router, tempfile::TempDir) {
        let agents = tempfile::tempdir().unwrap();
        write_test_blueprint(&agents.path().join("spawnable"), "spawnable");
        // A sibling subdir with no manifest, so blueprint discovery exercises the
        // "subdir without agent.leviath" branch.
        std::fs::create_dir_all(agents.path().join("not-a-blueprint")).unwrap();
        let state = test_state_with_agent_paths(vec![agents.path().to_path_buf()], control);
        let app = Router::new()
            .route("/api/agents", axum::routing::post(spawn_agent))
            .with_state(state);
        (app, agents)
    }

    async fn post_spawn(app: Router, body: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn spawn_agent_blueprint_not_found_returns_404() {
        let (app, _agents) = spawn_app(no_daemon());
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"ghost","task":"t"}"#).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn spawn_agent_success_returns_ok() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "run-1".to_string(),
        });
        let (app, _agents) = spawn_app(control);
        assert_eq!(
            post_spawn(
                app,
                r#"{"blueprint":"spawnable","task":"do it","workdir":"/tmp"}"#
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn spawn_agent_without_workdir_falls_back_to_cwd() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "r".to_string(),
        });
        let (app, _agents) = spawn_app(control);
        // No workdir field → the handler falls back to the current directory.
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn spawn_agent_daemon_error_returns_400() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Error {
            message: "bad blueprint".to_string(),
        });
        let (app, _agents) = spawn_app(control);
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn spawn_agent_unexpected_response_returns_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        let (app, _agents) = spawn_app(control);
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn spawn_agent_daemon_absent_returns_503() {
        let (app, _agents) = spawn_app(no_daemon());
        assert_eq!(
            post_spawn(app, r#"{"blueprint":"spawnable","task":"t"}"#).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    // ─── list_agents ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_agents_no_filter_returns_ok() {
        let app = Router::new()
            .route("/api/agents", get(list_agents))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn list_agents_with_status_filter_running() {
        crate::runstate::with_isolated_runs_dir_async(
            "list_agents_with_status_filter_running",
            |_d| async move {
                let run_id = unique_run_id("list-filter");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Running;
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents", get(list_agents))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri("/api/agents?status=running")
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let runs: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                // The run we created has Running status
                let found = runs.iter().any(|r| r.run_id == run_id);
                assert_found_running_run(found);

                // Cleanup
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    fn assert_found_running_run(found: bool) {
        assert!(found, "should find the running run");
    }

    #[test]
    #[should_panic(expected = "should find the running run")]
    fn assert_found_running_run_panics_when_not_found() {
        assert_found_running_run(false);
    }

    #[tokio::test]
    async fn list_agents_with_status_filter_excludes_others() {
        crate::runstate::with_isolated_runs_dir_async(
            "list_agents_with_status_filter_excludes_others",
            |_d| async move {
                let run_id = unique_run_id("list-filter-excl");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Complete;
                create_run(&meta).unwrap();

                // Create a second run with Running status so the filtered list is
                // non-empty - this makes the map/any closure in the assertion actually
                // execute, covering the closure body in LLVM's instrumentation.
                let run_id2 = unique_run_id("list-filter-excl-running");
                let mut meta2 = make_run(&run_id2);
                meta2.status = RunStatus::Running;
                create_run(&meta2).unwrap();

                let app = Router::new()
                    .route("/api/agents", get(list_agents))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri("/api/agents?status=running")
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let runs: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                // The complete run should not appear in the 'running' filter.
                let found = runs.iter().any(|r| r.run_id == run_id);
                assert_complete_run_excluded(found);
                // The running run should appear.
                let found2 = runs.iter().any(|r| r.run_id == run_id2);
                assert_running_run_included(found2);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id2));
            },
        )
        .await;
    }

    fn assert_complete_run_excluded(found: bool) {
        assert!(!found, "complete run should not appear in 'running' filter");
    }

    #[test]
    #[should_panic(expected = "complete run should not appear in 'running' filter")]
    fn assert_complete_run_excluded_panics_when_found() {
        assert_complete_run_excluded(true);
    }

    fn assert_running_run_included(found2: bool) {
        assert!(found2, "running run should appear in 'running' filter");
    }

    #[test]
    #[should_panic(expected = "running run should appear in 'running' filter")]
    fn assert_running_run_included_panics_when_not_found() {
        assert_running_run_included(false);
    }

    #[tokio::test]
    async fn list_agents_multi_status_filter() {
        crate::runstate::with_isolated_runs_dir_async(
            "list_agents_multi_status_filter",
            |_d| async move {
                let run_id = unique_run_id("list-multi");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Error;
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents", get(list_agents))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri("/api/agents?status=running,error")
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let runs: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                let found = runs.iter().any(|r| r.run_id == run_id);
                assert_error_run_included(found);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    fn assert_error_run_included(found: bool) {
        assert!(found, "error run should appear in 'running,error' filter");
    }

    #[test]
    #[should_panic(expected = "error run should appear in 'running,error' filter")]
    fn assert_error_run_included_panics_when_not_found() {
        assert_error_run_included(false);
    }

    // ─── get_agent ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_agent_existing_run_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "get_agent_existing_run_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("get-agent");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}", get(get_agent))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let got: RunMeta = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.run_id, run_id);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn get_agent_nonexistent_returns_404() {
        let app = Router::new()
            .route("/api/agents/{id}", get(get_agent))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents/totally-nonexistent-run-id-12345")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── agent_children ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_children_with_children_found() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_children_with_children_found",
            |_d| async move {
                let parent_id = unique_run_id("parent");
                let child_id = unique_run_id("child");

                let parent = make_run(&parent_id);
                create_run(&parent).unwrap();

                let mut child = make_run(&child_id);
                child.parent_run_id = Some(parent_id.clone());
                create_run(&child).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/children", get(agent_children))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/children", parent_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let children: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                assert_child_run_appears(children.iter().any(|c| c.run_id == child_id));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&parent_id));
                let _ = std::fs::remove_dir_all(runstate::run_dir(&child_id));
            },
        )
        .await;
    }

    fn assert_child_run_appears(found: bool) {
        assert!(found, "child run should appear");
    }

    #[test]
    #[should_panic(expected = "child run should appear")]
    fn assert_child_run_appears_panics_when_not_found() {
        assert_child_run_appears(false);
    }

    #[tokio::test]
    async fn agent_children_no_children_returns_empty() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_children_no_children_returns_empty",
            |_d| async move {
                let run_id = unique_run_id("no-children");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/children", get(agent_children))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/children", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let children: Vec<RunMeta> = serde_json::from_slice(&body).unwrap();
                assert_no_self_in_children(&children);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    fn assert_no_self_in_children(children: &[RunMeta]) {
        assert!(
            children.is_empty(),
            "run itself should not appear in its own children list"
        );
    }

    #[test]
    #[should_panic(expected = "run itself should not appear in its own children list")]
    fn assert_no_self_in_children_panics_when_nonempty() {
        assert_no_self_in_children(&[make_run("bogus-child")]);
    }

    // ─── agent_context ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_context_with_snapshot_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_with_snapshot_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("ctx-snap");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                // Write a context snapshot
                let snap = runstate::ContextSnapshot {
                    stage_name: "plan".to_string(),
                    total_tokens: 5000,
                    max_tokens: 200000,
                    regions: vec![],
                };
                runstate::write_context_snapshot(&run_id, &snap).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/context", get(agent_context))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let got: runstate::ContextSnapshot = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.total_tokens, 5000);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_context_no_snapshot_returns_404() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_no_snapshot_returns_404",
            |_d| async move {
                let run_id = unique_run_id("ctx-none");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/context", get(agent_context))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// Write a minimal `run.lvr` (Header + one ContextCheckpoint) for `run_id`.
    fn write_archive_fixture(run_id: &str) {
        write_archive_with_points(run_id, 1, None);
    }

    /// A journal with `points` context checkpoints, so history paging has
    /// something to page over. `secret` plants a webhook key in the header, for
    /// the redaction test.
    fn write_archive_with_points(run_id: &str, points: usize, secret: Option<&str>) {
        use leviath_core::run_archive::{self, RunIdentity, RunRecord};
        let mut meta = make_run(run_id);
        meta.callback_secret = secret.map(str::to_string);
        let mut buf = Vec::new();
        run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
        run_archive::write_record(
            &mut buf,
            &RunRecord::Header {
                identity: RunIdentity {
                    run_id: run_id.to_string(),
                    machine_id: "m".to_string(),
                    world_id: "w".to_string(),
                    created_at: 0,
                },
                meta: Box::new(meta),
            },
        )
        .unwrap();
        for i in 0..points {
            run_archive::write_record(
                &mut buf,
                &RunRecord::ContextCheckpoint {
                    snapshot: runstate::ContextSnapshot {
                        // Named by index so a test can tell which point it got.
                        stage_name: if points == 1 {
                            "plan".to_string()
                        } else {
                            format!("stage-{i}")
                        },
                        total_tokens: 7,
                        max_tokens: 100,
                        regions: vec![],
                    },
                    at: 1 + i as i64,
                },
            )
            .unwrap();
        }
        std::fs::write(runstate::run_dir(run_id).join("run.lvr"), &buf).unwrap();
    }

    /// Call the history route and return the decoded page.
    async fn history_page(run_id: &str, extra: &str) -> serde_json::Value {
        let app = Router::new()
            .route(
                "/api/agents/{id}/context/history",
                get(agent_context_history),
            )
            .with_state(test_state());
        let req = Request::builder()
            .uri(format!("/api/agents/{run_id}/context/history{extra}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn agent_context_history_returns_ok() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_history_returns_ok",
            |_d| async move {
                let run_id = unique_run_id("ctx-hist");
                create_run(&make_run(&run_id)).unwrap();
                write_archive_fixture(&run_id);

                let page = history_page(&run_id, "").await;
                assert_eq!(page["items"].as_array().unwrap().len(), 1);
                assert_eq!(page["items"][0]["context"]["stage_name"], "plan");
                assert_eq!(page["total"], 1);
                assert!(page["next_cursor"].is_null());

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// This route returned every recorded point, each carrying a full context
    /// window, on a journal that grows for as long as the run does.
    #[tokio::test]
    async fn context_history_pages_through_every_point_exactly_once() {
        crate::runstate::with_isolated_runs_dir_async("ctx-hist-pages", |_d| async move {
            let run_id = unique_run_id("ctx-page");
            create_run(&make_run(&run_id)).unwrap();
            write_archive_with_points(&run_id, 7, None);

            let mut seen: Vec<String> = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..10 {
                let extra = match cursor {
                    None => "?limit=3".to_string(),
                    Some(ref c) => format!("?limit=3&cursor={c}"),
                };
                let page = history_page(&run_id, &extra).await;
                assert_eq!(page["total"], 7);
                for item in page["items"].as_array().unwrap() {
                    seen.push(item["context"]["stage_name"].as_str().unwrap().to_string());
                }
                match page["next_cursor"].as_str() {
                    Some(c) => cursor = Some(c.to_string()),
                    None => break,
                }
            }

            // Chronological by default, complete, and nothing repeated.
            assert_eq!(
                seen,
                (0..7).map(|i| format!("stage-{i}")).collect::<Vec<_>>()
            );

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// Descending is what a UI tailing a run wants: the latest window without
    /// paging through the whole history to reach it.
    #[tokio::test]
    async fn context_history_can_start_from_the_most_recent_point() {
        crate::runstate::with_isolated_runs_dir_async("ctx-hist-desc", |_d| async move {
            let run_id = unique_run_id("ctx-desc");
            create_run(&make_run(&run_id)).unwrap();
            write_archive_with_points(&run_id, 5, None);

            let page = history_page(&run_id, "?order=desc&limit=2").await;
            let names: Vec<&str> = page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["context"]["stage_name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["stage-4", "stage-3"]);

            // And it continues downwards.
            let cursor = page["next_cursor"].as_str().unwrap();
            let next = history_page(&run_id, &format!("?order=desc&limit=2&cursor={cursor}")).await;
            let names: Vec<&str> = next["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["context"]["stage_name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["stage-2", "stage-1"]);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// The journal stores `RunMeta` whole, so every point carries the webhook
    /// signing key unless this route strips it. It did not, which is how the
    /// key was reachable by any token holder.
    #[tokio::test]
    async fn context_history_never_serves_the_webhook_secret() {
        crate::runstate::with_isolated_runs_dir_async("ctx-hist-secret", |_d| async move {
            let run_id = unique_run_id("ctx-secret");
            create_run(&make_run(&run_id)).unwrap();
            write_archive_with_points(&run_id, 2, Some("super-secret-signing-key"));

            let page = history_page(&run_id, "").await;
            assert!(!page.to_string().contains("super-secret-signing-key"));

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    #[tokio::test]
    async fn context_history_rejects_a_bad_order_or_limit() {
        crate::runstate::with_isolated_runs_dir_async("ctx-hist-bad", |_d| async move {
            let run_id = unique_run_id("ctx-bad");
            create_run(&make_run(&run_id)).unwrap();
            write_archive_fixture(&run_id);

            for extra in ["?order=sideways", "?limit=0", "?cursor=zzz"] {
                let app = Router::new()
                    .route(
                        "/api/agents/{id}/context/history",
                        get(agent_context_history),
                    )
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{run_id}/context/history{extra}"))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(
                    resp.status(),
                    axum::http::StatusCode::BAD_REQUEST,
                    "expected {extra} to be rejected"
                );
            }

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    #[tokio::test]
    async fn agent_context_history_no_archive_returns_404() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_context_history_no_archive_returns_404",
            |_d| async move {
                let run_id = unique_run_id("ctx-hist-none");
                create_run(&make_run(&run_id)).unwrap();

                let app = Router::new()
                    .route(
                        "/api/agents/{id}/context/history",
                        get(agent_context_history),
                    )
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/context/history", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    // ─── agent_logs ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_logs_reads_the_current_stage_not_the_dead_run_level_file() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_reads_current_stage",
            |_d| async move {
                let run_id = unique_run_id("logs-ok");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                // Two stages, so "current" has to mean the last one and not
                // just "the only one that exists".
                runstate::write_stages_index(
                    &run_id,
                    &[stage_rec(0, "plan"), stage_rec(1, "code")],
                )
                .unwrap();
                runstate::append_stage_output(&run_id, 0, "from the planning stage");
                runstate::append_stage_output(&run_id, 1, "from the coding stage");
                // The path the handler used to read. Nothing writes it in
                // production; planting it here proves the handler stopped.
                std::fs::write(
                    runstate::run_dir(&run_id).join("output.log"),
                    "the dead run-level file",
                )
                .unwrap();

                let body = logs_body(&run_id, "").await;
                assert!(body.contains("from the coding stage"));
                assert!(!body.contains("from the planning stage"));
                assert!(!body.contains("the dead run-level file"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_selects_a_stage_a_stream_and_every_stage() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_selects_stage_and_stream",
            |_d| async move {
                let run_id = unique_run_id("logs-select");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();
                runstate::write_stages_index(
                    &run_id,
                    &[stage_rec(0, "plan"), stage_rec(1, "code")],
                )
                .unwrap();
                runstate::append_stage_output(&run_id, 0, "plan output");
                runstate::append_stage_output(&run_id, 1, "code output");
                runstate::append_stage_log(&run_id, 1, "[tool] write_file: ok");

                // An explicit index reaches back past the current stage.
                let stage0 = logs_body(&run_id, "?stage=0").await;
                assert!(stage0.contains("plan output"));
                assert!(!stage0.contains("code output"));

                // The two streams stay separate.
                let operational = logs_body(&run_id, "?stream=logs").await;
                assert!(operational.contains("[tool] write_file: ok"));
                assert!(!operational.contains("code output"));

                // `all` joins every stage, oldest first, and labels them.
                let all = logs_body(&run_id, "?stage=all").await;
                assert!(all.contains("plan output"));
                assert!(all.contains("code output"));
                assert!(all.contains("stage 0: plan"));
                assert!(all.find("plan output") < all.find("code output"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_tail_bounds_the_bytes_returned() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_tail_bounds_bytes",
            |_d| async move {
                let run_id = unique_run_id("logs-tail");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();
                runstate::write_stages_index(&run_id, &[stage_rec(0, "only")]).unwrap();
                for i in 0..200 {
                    runstate::append_stage_output(&run_id, 0, &format!("line {i}"));
                }

                let full = logs_body(&run_id, "?tail=100000").await;
                assert!(full.contains("line 0"));

                // `tail` is a byte budget, so a small one drops the head.
                let tailed = logs_body(&run_id, "?tail=200").await;
                assert!(tailed.len() <= 200);
                assert!(tailed.contains("line 199"));
                assert!(!tailed.contains("line 0\n"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_falls_back_to_the_run_level_file_when_no_stages_exist() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_no_stages_fallback",
            |_d| async move {
                let run_id = unique_run_id("logs-fallback");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();
                // No stages.json at all - a run whose stage dirs were pruned.
                std::fs::write(
                    runstate::run_dir(&run_id).join("output.log"),
                    "legacy output",
                )
                .unwrap();

                assert!(logs_body(&run_id, "").await.contains("legacy output"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_rejects_an_unparseable_stage_or_stream() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_logs_rejects_bad_params",
            |_d| async move {
                let run_id = unique_run_id("logs-bad");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                for query in ["?stage=middle", "?stream=everything"] {
                    let app = Router::new()
                        .route("/api/agents/{id}/logs", get(agent_logs))
                        .with_state(test_state());
                    let req = Request::builder()
                        .uri(format!("/api/agents/{run_id}/logs{query}"))
                        .body(Body::empty())
                        .unwrap();
                    let resp = app.oneshot(req).await.unwrap();
                    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
                }

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_logs_nonexistent_run_returns_404() {
        let app = Router::new()
            .route("/api/agents/{id}/logs", get(agent_logs))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-xyz-logs/logs")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── agent_file ───────────────────────────────────────────────────────────

    fn files_app() -> Router {
        Router::new()
            .route("/api/agents/{id}/files", get(agent_file))
            .with_state(test_state())
    }

    /// A run whose workdir is `workdir`, persisted so `read_meta` finds it.
    fn create_run_in(id: &str, workdir: &std::path::Path) -> RunMeta {
        let mut meta = make_run(id);
        meta.workdir = workdir.to_string_lossy().to_string();
        create_run(&meta).unwrap();
        meta
    }

    /// GET the file with an explicit byte offset.
    async fn get_file_at(id: &str, path: &str, offset: u64) -> (StatusCode, Vec<u8>) {
        let uri = format!("/api/agents/{id}/files?path={path}&offset={offset}");
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = files_app().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    /// GET `/api/agents/{id}/files?path=<path>`, returning status and body.
    /// (`path` goes into the query string verbatim - every path these tests
    /// use is query-safe as-is.)
    async fn get_file(id: &str, path: &str) -> (StatusCode, Vec<u8>) {
        let uri = format!("/api/agents/{id}/files?path={path}");
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = files_app().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    fn error_of(body: &[u8]) -> String {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// `GET /api/agents/{id}/files` with no `path`, plus any extra query.
    async fn list_files(id: &str, extra: &str) -> (StatusCode, serde_json::Value) {
        let uri = format!("/api/agents/{id}/files{extra}");
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = files_app().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    /// The original Lair blocker: the console had a "+N more" badge and no
    /// endpoint that could list the names behind it.
    #[tokio::test]
    async fn listing_defaults_to_what_the_run_recorded_modifying() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_modified", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("kept.rs"), "x").unwrap();
            let run_id = unique_run_id("files-mod");

            let mut meta = make_run(&run_id);
            meta.workdir = workdir.path().to_string_lossy().into_owned();
            meta.flags.modified_files = vec!["kept.rs".to_string(), "deleted.rs".to_string()];
            // Three calls, two distinct files: the two numbers disagree, which
            // is exactly why a client must not subtract them.
            meta.flags.modified_file_count = 3;
            create_run(&meta).unwrap();

            let (status, listing) = list_files(&run_id, "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listing["source"], "modified");
            assert_eq!(listing["entries"].as_array().unwrap().len(), 2);
            // A path the run recorded but which is now gone is reported as
            // such rather than quietly dropped.
            assert_eq!(listing["entries"][0]["exists"], true);
            assert_eq!(listing["entries"][1]["name"], "deleted.rs");
            assert_eq!(listing["entries"][1]["exists"], false);
            // Named for what it counts, and not equal to the file count.
            assert_eq!(listing["modifying_tool_calls"], 3);
            assert_eq!(listing["modified_files_truncated"], false);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// Past the record-time cap the remaining names were never stored, so the
    /// only honest thing the API can do is say the list is a prefix.
    #[tokio::test]
    async fn a_run_at_the_tracking_cap_reports_its_list_as_truncated() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_capped", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            let run_id = unique_run_id("files-cap");
            let mut meta = make_run(&run_id);
            meta.workdir = workdir.path().to_string_lossy().into_owned();
            meta.flags.modified_files = (0..leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES)
                .map(|i| format!("f{i}.rs"))
                .collect();
            meta.flags.modified_file_count = 5_000;
            create_run(&meta).unwrap();

            let (_, listing) = list_files(&run_id, "").await;
            assert_eq!(listing["modified_files_truncated"], true);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// One directory level per request is the answer to "a repo with
    /// node_modules": the client walks the tree rather than one response
    /// trying to enumerate it.
    #[tokio::test]
    async fn a_workdir_listing_is_one_level_deep_and_descends_by_path() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_workdir", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("top.txt"), "x").unwrap();
            std::fs::write(workdir.path().join(".hidden"), "x").unwrap();
            std::fs::create_dir(workdir.path().join("nested")).unwrap();
            std::fs::write(workdir.path().join("nested/deep.txt"), "x").unwrap();
            let run_id = unique_run_id("files-wd");
            create_run_in(&run_id, workdir.path());

            let (status, listing) = list_files(&run_id, "?source=workdir").await;
            assert_eq!(status, StatusCode::OK);
            let names: Vec<&str> = listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap())
                .collect();
            // Directories first, then by name. The nested file is not here -
            // one level only.
            assert_eq!(names, vec!["nested", "top.txt"]);
            assert!(listing["parent"].is_null(), "at the workdir root");

            // Hidden entries are opt-in, mirroring the folder picker.
            let (_, with_hidden) = list_files(&run_id, "?source=workdir&hidden=true").await;
            assert_eq!(with_hidden["entries"].as_array().unwrap().len(), 3);

            // Descending is the same route with a path.
            let (_, deeper) = list_files(&run_id, "?source=workdir&path=nested").await;
            assert_eq!(deeper["entries"][0]["name"], "deep.txt");

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A lost workspace is a known run outcome, and an empty listing would
    /// read as "this run touched nothing".
    #[tokio::test]
    async fn a_workdir_listing_404s_when_the_workspace_is_gone() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_gone", |_d| async move {
            let run_id = unique_run_id("files-gone");
            let mut meta = make_run(&run_id);
            meta.workdir = "/definitely/not/here".to_string();
            create_run(&meta).unwrap();

            let (status, _) = list_files(&run_id, "?source=workdir").await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A recorded path can be absolute, because a tool can be handed one, and
    /// it can be shaped so it has no file name at all. Neither may panic or
    /// silently vanish from the list.
    #[tokio::test]
    async fn a_modified_listing_handles_absolute_and_nameless_paths() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_odd_paths", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("elsewhere.txt"), "x").unwrap();
            let run_id = unique_run_id("files-odd");

            let mut meta = make_run(&run_id);
            meta.workdir = workdir.path().to_string_lossy().into_owned();
            meta.flags.modified_files = vec![
                outside
                    .path()
                    .join("elsewhere.txt")
                    .to_string_lossy()
                    .into_owned(),
                // `Path::file_name` is None for a path ending in `..`.
                "nested/..".to_string(),
            ];
            create_run(&meta).unwrap();

            let (status, listing) = list_files(&run_id, "").await;
            assert_eq!(status, StatusCode::OK);
            let entries = listing["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 2);
            // The absolute one resolved, exists, and is flagged as outside the
            // fence rather than quietly dropped.
            assert_eq!(entries[0]["name"], "elsewhere.txt");
            assert_eq!(entries[0]["exists"], true);
            assert_eq!(entries[0]["outside_workdir"], true);
            // With no file name to show, the recorded path stands in for it.
            assert_eq!(entries[1]["name"], "nested/..");

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A single directory really can hold six figures of entries, and the
    /// response is built in memory.
    #[tokio::test]
    async fn a_workdir_listing_stops_at_the_entry_cap() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_cap", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            for i in 0..MAX_LISTING_ENTRIES + 5 {
                std::fs::write(workdir.path().join(format!("f{i}.txt")), "x").unwrap();
            }
            let run_id = unique_run_id("files-many");
            create_run_in(&run_id, workdir.path());

            let (status, listing) = list_files(&run_id, "?source=workdir").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                listing["entries"].as_array().unwrap().len(),
                MAX_LISTING_ENTRIES
            );
            assert_eq!(listing["truncated"], true);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A symlinked child can point outside the workdir even when the directory
    /// being listed is inside it, so containment is checked per entry.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_workdir_listing_excludes_a_child_that_escapes_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_escape", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("inside.txt"), "x").unwrap();
            std::os::unix::fs::symlink(outside.path(), workdir.path().join("escape")).unwrap();
            let run_id = unique_run_id("files-escape");
            create_run_in(&run_id, workdir.path());

            let (_, listing) = list_files(&run_id, "?source=workdir").await;
            let names: Vec<&str> = listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["inside.txt"]);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// Windows twin of the above. Windows spells a directory link
    /// `symlink_dir`; the fence behaves the same because `resolves_within`
    /// canonicalizes before comparing.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_workdir_listing_excludes_a_child_that_escapes_the_workdir_windows() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_escape", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("inside.txt"), "x").unwrap();
            std::os::windows::fs::symlink_dir(outside.path(), workdir.path().join("escape"))
                .unwrap();
            let run_id = unique_run_id("files-escape");
            create_run_in(&run_id, workdir.path());

            let (_, listing) = list_files(&run_id, "?source=workdir").await;
            let names: Vec<&str> = listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["inside.txt"]);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A journal that records no context change has no timeline to show, which
    /// is a 404 rather than an empty page - but only when the caller was not
    /// already partway through a walk.
    #[tokio::test]
    async fn context_history_404s_when_a_journal_records_no_points() {
        crate::runstate::with_isolated_runs_dir_async("ctx-hist-empty", |_d| async move {
            let run_id = unique_run_id("ctx-empty");
            create_run(&make_run(&run_id)).unwrap();
            // A header and nothing else: a valid archive with zero points.
            write_archive_with_points(&run_id, 0, None);

            let app = Router::new()
                .route(
                    "/api/agents/{id}/context/history",
                    get(agent_context_history),
                )
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/agents/{run_id}/context/history"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    #[tokio::test]
    async fn an_unknown_file_source_is_refused() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_bad_source", |_d| async move {
            let run_id = unique_run_id("files-bad");
            create_run(&make_run(&run_id)).unwrap();

            let (status, body) = list_files(&run_id, "?source=everything").await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["error"].as_str().unwrap().contains("everything"));

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// Reading a file must still serialize byte-for-byte as it did before the
    /// listing shape was added, or every existing client breaks.
    #[tokio::test]
    async fn reading_a_file_still_returns_the_original_shape() {
        crate::runstate::with_isolated_runs_dir_async("agent_files_compat", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("report.md"), "hello").unwrap();
            let run_id = unique_run_id("files-compat");
            create_run_in(&run_id, workdir.path());

            let (status, body) = get_file(&run_id, "report.md").await;
            assert_eq!(status, StatusCode::OK);
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let keys: Vec<&str> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, vec!["content", "path", "size", "truncated"]);
            assert_eq!(value["content"], "hello");

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    #[tokio::test]
    async fn agent_file_unknown_run_returns_404() {
        let (status, body) = get_file("nonexistent-run-xyz-files", "report.md").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            error_of(&body),
            "Agent run 'nonexistent-run-xyz-files' not found"
        );
    }

    #[tokio::test]
    async fn agent_file_reads_a_relative_path_within_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_reads_a_relative_path_within_the_workdir",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                std::fs::create_dir_all(workdir.path().join("notes")).unwrap();
                std::fs::write(workdir.path().join("notes/report.md"), "# Report\n").unwrap();
                let run_id = unique_run_id("file-rel");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "notes/report.md").await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.content, "# Report\n");
                assert_eq!(got.size, 9);
                assert!(!got.truncated);
                // The reported path is the resolved absolute one.
                assert!(std::path::Path::new(&got.path).is_absolute());
                assert!(got.path.ends_with("report.md"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_reads_an_absolute_path_within_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_reads_an_absolute_path_within_the_workdir",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let file = workdir.path().join("out.txt");
                std::fs::write(&file, "done").unwrap();
                let run_id = unique_run_id("file-abs");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, &file.to_string_lossy()).await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert_eq!(got.content, "done");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// `..` traversal and an unrelated absolute path both resolve outside the
    /// run's workdir, and both are refused before any read happens.
    #[tokio::test]
    async fn agent_file_refuses_a_path_outside_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_refuses_a_path_outside_the_workdir",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let run_id = unique_run_id("file-outside");
                create_run_in(&run_id, workdir.path());

                for outside in ["../escape.txt", "/etc/hosts"] {
                    let (status, body) = get_file(&run_id, outside).await;
                    assert_eq!(status, StatusCode::FORBIDDEN, "{outside}");
                    assert_eq!(
                        error_of(&body),
                        format!("path '{outside}' is outside the run's working directory")
                    );
                }

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// The containment is symlink-aware: a link planted under the workdir
    /// cannot be used to read outside it.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_file_is_not_fooled_by_a_symlink() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_is_not_fooled_by_a_symlink",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let outside = tempfile::tempdir().unwrap();
                std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
                std::os::unix::fs::symlink(outside.path(), workdir.path().join("escape")).unwrap();
                let run_id = unique_run_id("file-symlink");
                create_run_in(&run_id, workdir.path());

                let (status, _body) = get_file(&run_id, "escape/secret.txt").await;
                assert_eq!(status, StatusCode::FORBIDDEN);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_missing_file_returns_404() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_missing_file_returns_404",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let run_id = unique_run_id("file-missing");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "no-such.md").await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(error_of(&body), "file 'no-such.md' not found");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// A run whose workdir has since been deleted answers with a plain client
    /// error (the containment or the read refuses), never a 500.
    #[tokio::test]
    async fn agent_file_deleted_workdir_is_a_client_error() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_deleted_workdir_is_a_client_error",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let run_id = unique_run_id("file-gone-workdir");
                create_run_in(&run_id, workdir.path());
                drop(workdir); // the tempdir is removed here

                let (status, _body) = get_file(&run_id, "report.md").await;
                assert!(status.is_client_error(), "got {status}");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    /// A directory used to be a 400. Asking for one is the natural way to say
    /// "what is in here", so it lists instead - the behavior change this route
    /// deliberately makes.
    async fn agent_file_directory_lists_instead_of_erroring() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_directory_lists",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                std::fs::create_dir(workdir.path().join("sub")).unwrap();
                std::fs::write(workdir.path().join("sub/inner.txt"), "hi").unwrap();
                let run_id = unique_run_id("file-dir");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "sub").await;
                assert_eq!(status, StatusCode::OK);
                let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(listing["kind"], "listing");
                assert_eq!(listing["source"], "workdir");
                assert_eq!(listing["entries"][0]["name"], "inner.txt");
                // "Up one level" from a subdirectory is the workdir.
                assert!(listing["parent"].is_string());

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// A file the server cannot open (here: no read permission) is reported,
    /// not a 500.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_file_unreadable_file_is_reported() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_unreadable_file_is_reported",
            |_d| async move {
                use std::os::unix::fs::PermissionsExt;
                let workdir = tempfile::tempdir().unwrap();
                let file = workdir.path().join("locked.txt");
                std::fs::write(&file, "sealed").unwrap();
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
                let run_id = unique_run_id("file-locked");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "locked.txt").await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                let msg = error_of(&body);
                assert!(msg.starts_with("could not read 'locked.txt'"), "{msg}");

                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644));
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// Windows twin of `agent_file_unreadable_file_is_reported`.
    ///
    /// Windows has no "no read permission" bit `std::fs::Permissions` can
    /// set: a read-only file is still readable. A sharing violation is the
    /// equivalent split. Holding an exclusive (no-share) handle open leaves
    /// `std::fs::metadata` working - it opens with zero desired access and
    /// falls back to `FindFirstFileEx` on a sharing violation anyway - while
    /// `File::open`, which wants read access, is refused. That is the same
    /// metadata-succeeds/open-fails shape `0o000` gives the Unix test.
    /// Creating the file THROUGH the exclusive handle leaves no closed-file
    /// window for Defender or the indexer to grab it first, which is what
    /// `blueprints.rs`'s Windows twin found the hard way.
    #[cfg(windows)]
    #[tokio::test]
    async fn agent_file_unreadable_file_is_reported_windows() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_unreadable_file_is_reported_windows",
            |_d| async move {
                use std::fs::OpenOptions;
                use std::os::windows::fs::OpenOptionsExt;

                let workdir = tempfile::tempdir().unwrap();
                let file = workdir.path().join("locked.txt");
                let mut locked = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .share_mode(0)
                    .open(&file)
                    .unwrap();
                std::io::Write::write_all(&mut locked, b"sealed").unwrap();
                let run_id = unique_run_id("file-locked-win");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "locked.txt").await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                let msg = error_of(&body);
                assert!(msg.starts_with("could not read 'locked.txt'"), "{msg}");

                drop(locked);
                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_caps_the_read_at_one_mib() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_caps_the_read_at_one_mib",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                let full = MAX_FILE_READ_BYTES as usize + 100;
                std::fs::write(workdir.path().join("big.log"), vec![b'a'; full]).unwrap();
                let run_id = unique_run_id("file-big");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "big.log").await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert!(got.truncated);
                assert_eq!(got.size, full as u64);
                assert_eq!(got.content.len(), MAX_FILE_READ_BYTES as usize);

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// The cap landing mid-character does not make a text file "not text": the
    /// split character's leading bytes are dropped instead.
    #[tokio::test]
    async fn agent_file_truncation_mid_character_stays_text() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_truncation_mid_character_stays_text",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                // 'a' up to one byte short of the cap, then a 3-byte '€'
                // straddling it, then more text past it.
                let mut bytes = vec![b'a'; MAX_FILE_READ_BYTES as usize - 1];
                bytes.extend_from_slice("€ and more".as_bytes());
                std::fs::write(workdir.path().join("split.md"), &bytes).unwrap();
                let run_id = unique_run_id("file-split");
                create_run_in(&run_id, workdir.path());

                let (status, body) = get_file(&run_id, "split.md").await;
                assert_eq!(status, StatusCode::OK);
                let got: FileContentResp = serde_json::from_slice(&body).unwrap();
                assert!(got.truncated);
                assert_eq!(got.content.len(), MAX_FILE_READ_BYTES as usize - 1);
                assert!(got.content.ends_with('a'));

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_file_binary_returns_415() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_file_binary_returns_415",
            |_d| async move {
                let workdir = tempfile::tempdir().unwrap();
                std::fs::write(workdir.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
                // Invalid from early on AND larger than the cap: proves a big
                // binary is still called binary, not "truncated text".
                let mut big = vec![0xffu8; 16];
                big.resize(MAX_FILE_READ_BYTES as usize + 16, 0xff);
                std::fs::write(workdir.path().join("big.bin"), &big).unwrap();
                let run_id = unique_run_id("file-binary");
                create_run_in(&run_id, workdir.path());

                for name in ["blob.bin", "big.bin"] {
                    let (status, body) = get_file(&run_id, name).await;
                    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{name}");
                    assert_eq!(error_of(&body), format!("'{name}' is not a text file"));
                }

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    // ─── agent_result ─────────────────────────────────────────────────────────

    /// The endpoint served a 64 KiB log tail long before an agent could say
    /// "here is my answer". Both are returned now: the tail says what the run
    /// did, `final_output` says what it concluded.
    #[tokio::test]
    async fn agent_result_serves_the_submitted_answer_beside_the_log_tail() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_result_serves_the_submitted_answer",
            |_d| async move {
                let run_id = unique_run_id("result-answer");
                let answer = leviath_core::output::FinalOutput::new(
                    r#"{"root":{"component":"Card"}}"#,
                    Some("a2ui".to_string()),
                    "summary".to_string(),
                    99,
                );
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Complete;
                // The descriptor goes in `meta.json`; the bytes go beside it.
                meta.final_output = Some(answer.descriptor());
                create_run(&meta).unwrap();
                runstate::write_final_output(&runstate::run_dir(&run_id), &answer.content).unwrap();
                std::fs::write(
                    runstate::run_dir(&run_id).join("output.log"),
                    "ran some tools\n",
                )
                .unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/result", get(agent_result))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/result", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let output = &result["final_output"];
                // Byte-identical: an unrecognized format is served exactly as
                // the agent wrote it, with its label alongside for the UI.
                assert_eq!(
                    output["content"].as_str().unwrap(),
                    r#"{"root":{"component":"Card"}}"#
                );
                assert_eq!(output["format"].as_str().unwrap(), "a2ui");
                assert_eq!(output["stage"].as_str().unwrap(), "summary");
                assert!(!output["truncated"].as_bool().unwrap());
                // And the log tail is still there for callers that read it.
                assert!(
                    result["output"]
                        .as_str()
                        .unwrap()
                        .contains("ran some tools")
                );

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// A run that submitted nothing reports `null` rather than an empty string,
    /// so a consumer can tell "no answer" from "an empty answer".
    #[tokio::test]
    async fn agent_result_reports_no_answer_as_null() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_result_reports_no_answer_as_null",
            |_d| async move {
                let run_id = unique_run_id("result-none");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();
                let app = Router::new()
                    .route("/api/agents/{id}/result", get(agent_result))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/result", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(result["final_output"].is_null());

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    /// A client may ask for any shape. The label is carried through untouched,
    /// which is what lets a browser console ask for a2ui with no server support.
    #[test]
    fn a_spawn_request_carries_an_arbitrary_output_shape() {
        let body = SpawnAgentReq {
            blueprint: "x".to_string(),
            task: "t".to_string(),
            output_format: Some("a2ui".to_string()),
            output_instructions: Some("One card per finding.".to_string()),
            output_schema: Some(serde_json::json!({"type": "object"})),
            ..Default::default()
        };
        let spec = output_request(&body).expect("a shape was asked for");
        assert_eq!(spec.format.as_deref(), Some("a2ui"));
        assert_eq!(spec.instructions.as_deref(), Some("One card per finding."));
        assert_eq!(spec.schema, Some(serde_json::json!({"type": "object"})));
    }

    #[test]
    fn a_spawn_request_asking_for_nothing_leaves_the_blueprint_in_charge() {
        assert!(output_request(&SpawnAgentReq::default()).is_none());
    }

    #[tokio::test]
    async fn agent_result_existing_run_no_stages() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_result_existing_run_no_stages",
            |_d| async move {
                let run_id = unique_run_id("result-no-stages");
                let mut meta = make_run(&run_id);
                meta.status = RunStatus::Complete;
                create_run(&meta).unwrap();

                // Write some output.log content
                let log_path = runstate::run_dir(&run_id).join("output.log");
                std::fs::write(&log_path, "task complete\n").unwrap();

                let app = Router::new()
                    .route("/api/agents/{id}/result", get(agent_result))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/result", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(result["run_id"].as_str().unwrap(), run_id);
                // The word every other route uses, not `Display`'s `Complete`.
                assert_eq!(result["status"].as_str().unwrap(), "complete");

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_result_existing_run_with_stages() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_result_existing_run_with_stages",
            |_d| async move {
                let run_id = unique_run_id("result-stages");
                let meta = make_run(&run_id);
                create_run(&meta).unwrap();

                // Write a stages index and stage output
                let stages = vec![runstate::StageRecord::new("plan".to_string(), 0)];
                runstate::write_stages_index(&run_id, &stages).unwrap();
                runstate::append_stage_output(&run_id, 0, "stage output here");

                let app = Router::new()
                    .route("/api/agents/{id}/result", get(agent_result))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/agents/{}/result", run_id))
                    .body(Body::empty())
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(result["run_id"].as_str().unwrap(), run_id);
                assert!(
                    result["output"]
                        .as_str()
                        .unwrap()
                        .contains("stage output here")
                );

                let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_result_nonexistent_run_returns_404() {
        let app = Router::new()
            .route("/api/agents/{id}/result", get(agent_result))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-xyz-result/result")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── kill_agent ───────────────────────────────────────────────────────────

    async fn delete_agent(control: ControlClient, id: &str) -> StatusCode {
        use axum::routing::delete;
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        };
        let app = Router::new()
            .route("/api/agents/{id}", delete(kill_agent))
            .with_state(state);
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/agents/{id}"))
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn kill_agent_cancels_via_daemon() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        assert_eq!(delete_agent(control, "run-a").await, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn kill_agent_unknown_run_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(delete_agent(control, "ghost").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn kill_agent_unexpected_response_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            delete_agent(control, "a").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn kill_agent_daemon_absent_is_503() {
        assert_eq!(
            delete_agent(no_daemon(), "a").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ─── pause_agent / resume_agent ──────────────────────────────────────────

    /// POST `/api/agents/{id}/pause` (or `/resume`) against a router holding
    /// `control`, returning the response status.
    async fn post_agent_action(control: ControlClient, id: &str, action: &str) -> StatusCode {
        use axum::routing::post;
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            update_check: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control,
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        };
        let app = Router::new()
            .route("/api/agents/{id}/pause", post(pause_agent))
            .route("/api/agents/{id}/resume", post(resume_agent))
            .with_state(state);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/agents/{id}/{action}"))
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn pause_agent_sends_pause_to_the_daemon() {
        let (control, _dir, _srv) = fake_daemon(|req| {
            assert_eq!(
                std::mem::discriminant(&req),
                std::mem::discriminant(&ControlRequest::Pause {
                    run_id: String::new()
                })
            );
            ControlResponse::Ok { ok: true }
        });
        assert_eq!(
            post_agent_action(control, "run-a", "pause").await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn pause_agent_refused_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(
            post_agent_action(control, "ghost", "pause").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn pause_agent_unexpected_response_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            post_agent_action(control, "a", "pause").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn pause_agent_daemon_absent_is_503() {
        assert_eq!(
            post_agent_action(no_daemon(), "a", "pause").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn resume_agent_sends_resume_to_the_daemon() {
        let (control, _dir, _srv) = fake_daemon(|req| {
            assert_eq!(
                std::mem::discriminant(&req),
                std::mem::discriminant(&ControlRequest::Resume {
                    run_id: String::new()
                })
            );
            ControlResponse::Ok { ok: true }
        });
        assert_eq!(
            post_agent_action(control, "run-a", "resume").await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn resume_agent_refused_is_404() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        assert_eq!(
            post_agent_action(control, "ghost", "resume").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn resume_agent_unexpected_response_is_500() {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "x".to_string(),
        });
        assert_eq!(
            post_agent_action(control, "a", "resume").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn resume_agent_daemon_absent_is_503() {
        assert_eq!(
            post_agent_action(no_daemon(), "a", "resume").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn spawn_agent_req_deserialization_minimal() {
        let json = r#"{
            "blueprint": "coder",
            "task": "write a hello world"
        }"#;
        let req: SpawnAgentReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.blueprint, "coder");
        assert_eq!(req.task, "write a hello world");
        assert!(req.model.is_none());
        assert!(req.workdir.is_none());
        assert!(!req.yolo);
        assert!(req.allow.is_empty());
        assert!(req.max_depth.is_none());
        assert!(req.metadata.is_empty());
        assert!(req.callback_url.is_none());
        assert!(req.callback_secret.is_none());
    }

    #[test]
    fn spawn_agent_req_deserialization_full() {
        let json = r#"{
            "blueprint": "coder",
            "task": "build app",
            "model": "claude-sonnet-4-6",
            "max_depth": 3,
            "yolo": true,
            "allow": ["read_file", "bash"],
            "workdir": "/tmp/work",
            "metadata": {"project": "test"},
            "callback_url": "https://example.com/hook",
            "callback_secret": "s3cret"
        }"#;
        let req: SpawnAgentReq = serde_json::from_str(json).unwrap();
        assert_eq!(req.blueprint, "coder");
        assert_eq!(req.model.unwrap(), "claude-sonnet-4-6");
        assert_eq!(req.max_depth, Some(3));
        assert!(req.yolo);
        assert_eq!(req.allow.len(), 2);
        assert_eq!(req.workdir.unwrap(), "/tmp/work");
        assert_eq!(req.metadata.get("project").unwrap(), "test");
        assert_eq!(req.callback_url.unwrap(), "https://example.com/hook");
        assert_eq!(req.callback_secret.unwrap(), "s3cret");
    }

    #[test]
    fn spawn_agent_resp_serialization() {
        let resp = SpawnAgentResp {
            agent_id: "coder".to_string(),
            run_id: "run-abc-123".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"agent_id\":\"coder\""));
        assert!(json.contains("\"run_id\":\"run-abc-123\""));
    }

    #[test]
    fn list_agents_query_deserialization_empty() {
        let json = "{}";
        let query: ListAgentsQuery = serde_json::from_str(json).unwrap();
        assert!(query.status.is_none());
    }

    #[test]
    fn list_agents_query_deserialization_with_status() {
        let json = r#"{"status": "running,complete"}"#;
        let query: ListAgentsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.status.unwrap(), "running,complete");
    }

    #[test]
    fn agent_result_resp_serialization() {
        let resp = AgentResultResp {
            run_id: "run-123".to_string(),
            status: "complete".to_string(),
            output: "done!".to_string(),
            error: None,
            prompt_tokens: 5000,
            completion_tokens: 1200,
            final_output: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"run_id\":\"run-123\""));
        assert!(json.contains("\"status\":\"complete\""));
        assert!(json.contains("\"prompt_tokens\":5000"));
        assert!(json.contains("\"completion_tokens\":1200"));
    }

    #[test]
    fn agent_result_resp_with_error() {
        let resp = AgentResultResp {
            run_id: "run-err".to_string(),
            status: "error".to_string(),
            output: String::new(),
            error: Some("something went wrong".to_string()),
            prompt_tokens: 100,
            completion_tokens: 0,
            final_output: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("something went wrong"));
    }

    #[test]
    fn logs_query_deserialization() {
        let json = r#"{"tail": 8192}"#;
        let query: LogsQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.tail, Some(8192));
    }

    #[test]
    fn logs_query_deserialization_empty() {
        let json = "{}";
        let query: LogsQuery = serde_json::from_str(json).unwrap();
        assert!(query.tail.is_none());
    }

    #[test]
    fn error_response_serialization() {
        let err = ErrorResponse {
            error: "not found".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"error\":\"not found\""));
    }

    /// A dataset can be far larger than one response, and the whole point of the
    /// artifact story is that a caller can actually get it. Reading it back a
    /// window at a time must reconstruct the file exactly.
    #[tokio::test]
    async fn a_large_artifact_can_be_paged_back_in_full() {
        crate::runstate::with_isolated_runs_dir_async("files_paging", |_d| async move {
            let work = tempfile::tempdir().unwrap();
            // Comfortably past the per-response cap.
            let original: String = (0..300_000).map(|i| format!("row {i}\n")).collect();
            assert!(
                original.len() as u64 > MAX_FILE_READ_BYTES * 2,
                "needs 3+ pages"
            );
            std::fs::write(work.path().join("data.csv"), &original).unwrap();
            let run_id = unique_run_id("files-paging");
            create_run_in(&run_id, work.path());

            let mut assembled = String::new();
            let mut offset = 0u64;
            let mut pages = 0;
            loop {
                let (status, body) = get_file_at(&run_id, "data.csv", offset).await;
                assert_eq!(status, StatusCode::OK);
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assembled.push_str(v["content"].as_str().unwrap());
                assert_eq!(v["size"].as_u64().unwrap(), original.len() as u64);
                pages += 1;
                match v["next_offset"].as_u64() {
                    Some(next) => offset = next,
                    None => {
                        assert!(!v["truncated"].as_bool().unwrap(), "last page is complete");
                        break;
                    }
                }
                assert!(pages < 20, "should not take this many pages");
            }
            assert!(
                pages >= 3,
                "the fixture must actually span pages, got {pages}"
            );
            assert_eq!(assembled, original, "the pages reassemble the file exactly");

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A window boundary can land inside a multi-byte character. The next page
    /// must resume on a boundary, or concatenating pages would corrupt the text.
    #[tokio::test]
    async fn paging_does_not_split_a_multi_byte_character() {
        crate::runstate::with_isolated_runs_dir_async("files_paging_utf8", |_d| async move {
            let work = tempfile::tempdir().unwrap();
            // Three-byte characters, so most offsets land mid-character.
            let original = "日本語".repeat(20);
            std::fs::write(work.path().join("t.txt"), &original).unwrap();
            let run_id = unique_run_id("files-utf8");
            create_run_in(&run_id, work.path());

            // Offset 1 is inside the first character.
            let (status, body) = get_file_at(&run_id, "t.txt", 1).await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            // The window was moved forward to the next boundary and says so, so
            // the caller is never handed half a character.
            assert_eq!(v["offset"].as_u64().unwrap(), 3);
            assert!(v["content"].as_str().unwrap().starts_with('本'));

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    #[tokio::test]
    async fn an_offset_past_the_end_is_refused_rather_than_returning_nothing() {
        crate::runstate::with_isolated_runs_dir_async("files_paging_past_end", |_d| async move {
            let work = tempfile::tempdir().unwrap();
            std::fs::write(work.path().join("s.txt"), "short").unwrap();
            let run_id = unique_run_id("files-past-end");
            create_run_in(&run_id, work.path());

            let (status, _) = get_file_at(&run_id, "s.txt", 9_999).await;
            assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    /// A small file reads whole with no offset, exactly as before.
    #[tokio::test]
    async fn a_small_file_still_reads_in_one_request() {
        crate::runstate::with_isolated_runs_dir_async("files_paging_small", |_d| async move {
            let work = tempfile::tempdir().unwrap();
            std::fs::write(work.path().join("s.txt"), "a,b\n1,2\n").unwrap();
            let run_id = unique_run_id("files-small");
            create_run_in(&run_id, work.path());

            let (status, body) = get_file(&run_id, "s.txt").await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["content"].as_str().unwrap(), "a,b\n1,2\n");
            assert!(!v["truncated"].as_bool().unwrap());
            assert!(v["next_offset"].is_null(), "nothing more to fetch");

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        })
        .await;
    }

    // ─── GET /api/agents/{id}/stages (#388) ──────────────────────────────────

    /// Build a ledger with one stage that ran, one the run stepped over, and
    /// one still ahead of it.
    fn ledger_fixture() -> Vec<leviath_core::run_meta::StageRecord> {
        use leviath_core::run_meta::{StageRecord, StageRunStatus};
        let mut plan = StageRecord::new("plan".to_string(), 0);
        plan.status = StageRunStatus::Complete;
        plan.entered = true;
        plan.prompt_tokens = 900;
        plan.completion_tokens = 120;
        plan.cached_tokens = 400;
        plan.cache_write_tokens = 60;
        plan.region_tokens.insert("task".to_string(), 24);
        plan.region_tokens.insert("data_preview".to_string(), 4004);
        plan.runaway_warned = true;

        let mut recovery = StageRecord::new("error_recovery".to_string(), 1);
        recovery.status = StageRunStatus::Skipped;

        let answer = StageRecord::new("answer".to_string(), 2);
        vec![plan, recovery, answer]
    }

    /// The route the issue asks for: the per-stage ledger, served as recorded.
    #[tokio::test]
    async fn agent_stages_serves_the_per_stage_ledger() {
        crate::runstate::with_isolated_runs_dir_async("agent_stages_serves", |_d| async move {
            let run_id = unique_run_id("stages-ledger");
            create_run(&make_run(&run_id)).unwrap();
            runstate::write_stages_index(&run_id, &ledger_fixture()).unwrap();

            let app = Router::new()
                .route("/api/agents/{id}/stages", get(agent_stages))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/agents/{}/stages", run_id))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);

            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["run_id"], run_id);

            let stages = body["stages"].as_array().expect("an array of stages");
            assert_eq!(stages.len(), 3);

            // The costs no other route can answer.
            assert_eq!(stages[0]["name"], "plan");
            assert_eq!(stages[0]["prompt_tokens"], 900);
            assert_eq!(stages[0]["cache_write_tokens"], 60);
            assert_eq!(stages[0]["region_tokens"]["data_preview"], 4004);
            assert_eq!(stages[0]["runaway_warned"], true);

            // The distinction that cannot be reconstructed from context/history:
            // a stage the run stepped over, versus one still ahead of it.
            assert_eq!(stages[1]["entered"], false);
            assert_eq!(stages[2]["entered"], false);
        })
        .await;
    }

    /// The wire spelling is snake_case, and pinned here because the issue is
    /// right that this has drifted before: `RunMeta` serializes snake_case
    /// while the result and tree routes render a PascalCase `Display`, and that
    /// asymmetry has already cost one filter bug.
    #[tokio::test]
    async fn agent_stages_spells_status_in_snake_case() {
        crate::runstate::with_isolated_runs_dir_async("agent_stages_spelling", |_d| async move {
            let run_id = unique_run_id("stages-spelling");
            create_run(&make_run(&run_id)).unwrap();
            runstate::write_stages_index(&run_id, &ledger_fixture()).unwrap();

            let app = Router::new()
                .route("/api/agents/{id}/stages", get(agent_stages))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/agents/{}/stages", run_id))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(body["stages"][0]["status"], "complete");
            assert_eq!(
                body["stages"][1]["status"], "skipped",
                "the status #372 added has to reach the wire under the name the \
                 schema advertises"
            );
            assert_eq!(body["stages"][2]["status"], "pending");
        })
        .await;
    }

    /// A run that has not reached its first stage boundary has no records yet.
    /// That is an empty list, not a 404 - the run exists, and answering "no
    /// such run" would send a client back to re-ask a settled question.
    #[tokio::test]
    async fn agent_stages_of_a_run_with_no_index_is_empty() {
        crate::runstate::with_isolated_runs_dir_async("agent_stages_empty", |_d| async move {
            let run_id = unique_run_id("stages-empty");
            create_run(&make_run(&run_id)).unwrap();

            let app = Router::new()
                .route("/api/agents/{id}/stages", get(agent_stages))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/agents/{}/stages", run_id))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);

            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(body["stages"].as_array().expect("an array").is_empty());
        })
        .await;
    }

    /// A run that does not exist is still a 404, or the empty-list answer above
    /// would swallow a typo'd id.
    #[tokio::test]
    async fn agent_stages_of_an_unknown_run_is_not_found() {
        crate::runstate::with_isolated_runs_dir_async("agent_stages_unknown", |_d| async move {
            let app = Router::new()
                .route("/api/agents/{id}/stages", get(agent_stages))
                .with_state(test_state());
            let req = Request::builder()
                .uri("/api/agents/no-such-run/stages")
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        })
        .await;
    }
}
