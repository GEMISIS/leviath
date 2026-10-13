//! A run's stored parts, its raw files, and the mime registry, over HTTP.
//!
//! `GET /api/runs/{id}/blobs` lists every stored part the run's context
//! holds, by hash, with what the context says about it: name, type, size,
//! dimensions, and the regions carrying it. `GET .../blobs/{sha256}` serves
//! the bytes under their own `Content-Type`, and `?download=1` asks the
//! browser to save rather than show. `GET .../files/raw?path=` serves a
//! workdir file the same way, typed by the registry. `GET /api/mime` is
//! the effective registry: every row and where it came from.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use leviath_core::mime::{MimeRegistry, MimeType, TokenRule, is_sha256_hex};
use serde::{Deserialize, Serialize};

use super::core::error::ServeError;
use super::types::*;
use crate::blobs::BlobEntry;
use crate::daemon::config_reload::ConfigReloader;
use crate::runstate;

/// The stored parts a run's context holds, or 404 when it has no run file.
fn stored_parts(run_id: &str) -> Result<Vec<BlobEntry>, ServeError> {
    super::core::inspect::blobs(run_id)
}

/// Every stored part a run holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct BlobListing {
    /// The parts, first appearance first.
    pub(super) items: Vec<BlobEntry>,
}

/// `GET /api/exports/{id}`: the JSONL an export wrote.
///
/// A byte route like the others, which is why a signed link opens it: an export
/// is downloaded by a browser, and a browser cannot put a header on a download.
pub(super) async fn export_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, ServeError> {
    use super::core::export::ExportStatus;

    let job = state.caches.exports.get(&id).ok_or_else(|| {
        ServeError::NotFound(format!(
            "no export '{id}': it was never started, or it has expired"
        ))
    })?;
    match job.status {
        ExportStatus::Complete => {}
        // Not a failure of this request: the export is simply not ready, and
        // the client polls the job rather than the file.
        ExportStatus::Queued | ExportStatus::Running => {
            return Err(ServeError::Conflict(format!(
                "export '{id}' is still {}",
                job.status.wire()
            )));
        }
        ExportStatus::Failed => {
            return Err(ServeError::Conflict(format!(
                "export '{id}' failed: {}",
                job.error
                    .unwrap_or_else(|| "no reason recorded".to_string())
            )));
        }
    }
    // Opened, not read: the file goes out a block at a time, only the range
    // asked for.
    let path = super::core::export::export_path(&id);
    let opened = super::blocking::blocking(move || {
        std::fs::File::open(&path).and_then(|file| file.metadata().map(|meta| (file, meta.len())))
    })
    .await
    .map_err(|e| {
        ServeError::Internal(format!(
            "export '{id}' is complete, but its file cannot be read: {e}"
        ))
    })?;
    // A literal this crate writes, so there is no failure to report: parsing it
    // is how the response's own type is built rather than a claim about input.
    let jsonl = MimeType::parse("application/jsonl").expect("jsonl is a mime type");
    bytes_response(
        Payload::File(opened.0, opened.1),
        &jsonl,
        &format!("{id}.jsonl"),
        true,
        headers
            .get(axum::http::header::RANGE)
            .and_then(|value| value.to_str().ok()),
    )
}

/// `GET /api/runs/{id}/blobs`: every stored part the run holds.
pub(super) async fn list_blobs(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<BlobListing>, ServeError> {
    // The run file is replayed to its last step on the blocking pool.
    let items = super::blocking::blocking(move || stored_parts(&id)).await?;
    Ok(Json(BlobListing { items }))
}

/// A query flag as browsers and shells spell it: `1`, `true`, `yes` and `on`
/// are on; anything else, and an absent value, is off.
fn flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let word = String::deserialize(d)?;
    Ok(matches!(
        word.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    ))
}

/// `?download=1` on the byte routes.
#[derive(Debug, Deserialize, Default)]
pub(super) struct BytesQuery {
    /// Ask the browser to save the file rather than show it.
    #[serde(default, deserialize_with = "flag")]
    pub(super) download: bool,
}

/// The `Range` request header as a string, when the caller sent one.
fn range_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::RANGE).and_then(|v| v.to_str().ok())
}

/// What a `Range` header asks for against a body of `total` bytes.
#[derive(Debug, PartialEq, Eq)]
enum RangeResult {
    /// No range, or one this route does not honor: send the whole body.
    Full,
    /// An inclusive byte range `[start, end]` to send as `206`.
    Partial(u64, u64),
    /// A range that names bytes the body does not have: `416`.
    Unsatisfiable,
}

/// Resolve a single-range `Range: bytes=...` header against a `total`-byte
/// body. A missing header, an unknown unit, a multi-range list, or a
/// malformed spec all read as [`RangeResult::Full`] - RFC 7233 says an
/// unsatisfiable *parse* is ignored and the whole body served. A well-formed
/// range past the end is [`RangeResult::Unsatisfiable`].
fn resolve_range(header: Option<&str>, total: u64) -> RangeResult {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return RangeResult::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeResult::Full;
    }
    let Some((from, to)) = spec.split_once('-') else {
        return RangeResult::Full;
    };
    let (start, end) = if from.is_empty() {
        // `-N`: the last N bytes.
        let Ok(n) = to.parse::<u64>() else {
            return RangeResult::Full;
        };
        if n == 0 {
            return RangeResult::Unsatisfiable;
        }
        (total.saturating_sub(n), total.saturating_sub(1))
    } else {
        let Ok(start) = from.parse::<u64>() else {
            return RangeResult::Full;
        };
        let end = match to.is_empty() {
            true => total.saturating_sub(1),
            false => match to.parse::<u64>() {
                Ok(e) => e.min(total.saturating_sub(1)),
                Err(_) => return RangeResult::Full,
            },
        };
        (start, end)
    };
    if total == 0 || start >= total || start > end {
        return RangeResult::Unsatisfiable;
    }
    RangeResult::Partial(start, end)
}

/// A file at most this big is read whole and typed from all of its bytes; a
/// bigger one is sent from disk, only the range asked for, a block at a time.
const LOADED_BYTES: u64 = 1 << 20;

/// How much of a bigger file is read to sniff its type.
const SNIFF_BYTES: u64 = 8 << 10;

/// How much of a file a streamed body reads at a time.
const BLOCK: usize = 64 << 10;

/// What a byte route sends.
#[derive(Debug)]
enum Payload {
    /// Bytes in memory.
    Loaded(Vec<u8>),
    /// An open file of this many bytes, read for the range sent and no more.
    File(std::fs::File, u64),
}

impl Payload {
    /// How many bytes the whole body is.
    fn len(&self) -> u64 {
        match self {
            Self::Loaded(bytes) => bytes.len() as u64,
            Self::File(_, len) => *len,
        }
    }

    /// The `len` bytes from `start`, as a response body.
    fn window(self, start: u64, len: u64) -> axum::body::Body {
        match self {
            Self::Loaded(mut bytes) => {
                bytes.truncate((start + len) as usize);
                bytes.drain(..start as usize);
                axum::body::Body::from(bytes)
            }
            Self::File(mut file, _) => {
                use std::io::Seek as _;
                use tokio::io::AsyncReadExt as _;
                // `start` is inside the file, where a regular file always
                // seeks.
                let _ = file.seek(std::io::SeekFrom::Start(start));
                let reader = tokio::fs::File::from_std(file).take(len);
                axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                    reader, BLOCK,
                ))
            }
        }
    }
}

/// The file at `path` as a body, typed the way
/// [`MimeRegistry::resolve`] types bytes, with `known` taking precedence: a
/// file of at most [`LOADED_BYTES`] is read whole, and a bigger one is
/// sniffed from its head and read through for UTF-8 only when neither that
/// nor `name` settles its type. `None` when it does not open and read as a
/// file.
fn file_body(
    path: &std::path::Path,
    name: Option<&str>,
    registry: &MimeRegistry,
    known: Option<MimeType>,
) -> Option<(Payload, MimeType)> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= LOADED_BYTES {
        let mut bytes = Vec::with_capacity(len as usize);
        (&file).read_to_end(&mut bytes).ok()?;
        let mime_type = known.unwrap_or_else(|| registry.resolve(None, name, &bytes));
        return Some((Payload::Loaded(bytes), mime_type));
    }
    let mut head = Vec::new();
    // A head cut short by a failed read is sniffed as far as it goes.
    let _ = (&file).take(SNIFF_BYTES).read_to_end(&mut head);
    let mime_type =
        known.unwrap_or_else(|| registry.resolve_with(None, name, &head, || is_utf8(&file)));
    Some((Payload::File(file, len), mime_type))
}

/// Whether `file` is valid UTF-8 from its start to its end, read a block at
/// a time. A character split across two blocks is carried over to the next.
fn is_utf8(mut file: &std::fs::File) -> bool {
    use std::io::{Read as _, Seek as _};
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let read = file
            .take(BLOCK as u64)
            .read_to_end(&mut pending)
            .unwrap_or(0);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            // Cut off mid-character: keep the start of it for the next block.
            Err(e) if e.error_len().is_none() => {
                pending.drain(..e.valid_up_to());
            }
            Err(_) => return false,
        }
        // At the end, a character still cut off is not valid UTF-8.
        if read == 0 {
            return pending.is_empty();
        }
    }
}

/// A body with its type, a download hint when asked for, and single-range
/// support so a client can seek (video scrubbing, resuming a download). Every
/// response advertises `Accept-Ranges: bytes`.
fn bytes_response(
    body: Payload,
    mime_type: &MimeType,
    name: &str,
    download: bool,
    range: Option<&str>,
) -> Result<Response, ServeError> {
    let total = body.len();
    let (status, body, content_range) = match resolve_range(range, total) {
        RangeResult::Full => (StatusCode::OK, body.window(0, total), None),
        RangeResult::Partial(start, end) => (
            StatusCode::PARTIAL_CONTENT,
            body.window(start, end - start + 1),
            Some(format!("bytes {start}-{end}/{total}")),
        ),
        RangeResult::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            axum::body::Body::empty(),
            Some(format!("bytes */{total}")),
        ),
    };
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime_type.as_str()).expect("a mime type is printable ASCII"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(cr) = content_range {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&cr).expect("a byte range is printable ASCII"),
        );
    }
    if download {
        let safe: String = name
            .chars()
            .map(
                |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    true => c,
                    false => '_',
                },
            )
            .collect();
        let disposition = format!("attachment; filename=\"{safe}\"");
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&disposition).expect("a sanitised name is printable ASCII"),
        );
    }
    Ok(response)
}

/// What a byte route serves, with the type and the name it goes out under.
struct Typed {
    body: Payload,
    mime_type: MimeType,
    name: String,
}

impl Typed {
    /// The response for this body, as [`bytes_response`] shapes it.
    fn respond(self, download: bool, headers: &HeaderMap) -> Result<Response, ServeError> {
        bytes_response(
            self.body,
            &self.mime_type,
            &self.name,
            download,
            range_header(headers),
        )
    }
}

/// `GET /api/runs/{id}/blobs/{sha256}`: the bytes of one stored part.
pub(super) async fn get_blob(
    State(state): State<AppState>,
    AxumPath((id, sha256)): AxumPath<(String, String)>,
    Query(query): Query<BytesQuery>,
    headers: HeaderMap,
) -> Result<Response, ServeError> {
    if !is_sha256_hex(&sha256) {
        return Err(ServeError::BadRequest(
            "a blob is named by its 64-character lowercase hex sha256".to_string(),
        ));
    }
    // The record, the part's file, the run file and the registry are all read
    // on the blocking pool.
    let config = Arc::clone(&state.config);
    super::blocking::blocking(move || blob_bytes(&config, &id, &sha256))
        .await?
        .respond(query.download, &headers)
}

/// The bytes of run `id`'s stored part `sha256`, typed and named.
fn blob_bytes(config: &ConfigReloader, id: &str, sha256: &str) -> Result<Typed, ServeError> {
    runstate::read_meta(id).map_err(|_| ServeError::run_not_found(id))?;
    // What the context says about it, when it says anything: the type it
    // was stored as and the name it carries. A blob the context no longer
    // references is typed by sniffing, and named by its hash.
    let known = stored_parts(id)
        .unwrap_or_default()
        .into_iter()
        .find(|b| b.sha256 == sha256);
    let registry = config.check().config.mime_registry_or_defaults();
    let declared = known
        .as_ref()
        .and_then(|b| MimeType::parse(&b.mime_type).ok());
    let path = super::core::inspect::blob_path(id, sha256);
    // A part whose path is there but does not read as a file is an error,
    // not a miss.
    let (body, mime_type) =
        file_body(&path, None, &registry, declared).ok_or_else(|| match path.exists() {
            true => ServeError::Internal(format!(
                "the stored part {sha256} of run '{id}' does not read as a file"
            )),
            false => ServeError::NotFound(format!(
                "run '{id}' has no stored part {sha256} in its blob directory"
            )),
        })?;
    let name = crate::blobs::export_name(
        known.as_ref().and_then(|b| b.name.as_deref()),
        sha256,
        mime_type.as_str(),
        &registry,
    );
    Ok(Typed {
        body,
        mime_type,
        name,
    })
}

/// `?path=` and `?download=` on the raw file route.
#[derive(Debug, Deserialize)]
pub(super) struct RawFileQuery {
    /// The file, relative to the run's workdir, or absolute inside it.
    pub(super) path: String,
    /// Ask the browser to save the file rather than show it.
    #[serde(default, deserialize_with = "flag")]
    pub(super) download: bool,
}

/// `GET /api/runs/{id}/files/raw?path=`: a workdir file's bytes, typed.
pub(super) async fn raw_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<RawFileQuery>,
    headers: HeaderMap,
) -> Result<Response, ServeError> {
    // The record, the fence, the file and the registry are all read on the
    // blocking pool.
    let config = Arc::clone(&state.config);
    let path = query.path.clone();
    super::blocking::blocking(move || raw_bytes(&config, &id, &path))
        .await?
        .respond(query.download, &headers)
}

/// The bytes of the file at `path` in run `id`'s workdir, typed by the
/// registry and named by the file.
fn raw_bytes(config: &ConfigReloader, id: &str, path: &str) -> Result<Typed, ServeError> {
    let meta = runstate::read_meta(id).map_err(|_| ServeError::run_not_found(id))?;
    let workdir = PathBuf::from(&meta.workdir);
    let requested = PathBuf::from(path);
    let resolved = match requested.is_absolute() {
        true => requested,
        false => workdir.join(&requested),
    };
    if !leviath_core::resolves_within(&resolved, &workdir) {
        return Err(ServeError::Forbidden(format!(
            "path '{path}' is outside the run's working directory"
        )));
    }
    if resolved.is_dir() {
        return Err(ServeError::BadRequest(format!(
            "'{path}' is a directory; the raw route serves files"
        )));
    }
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let registry = config.check().config.mime_registry_or_defaults();
    let (body, mime_type) = match file_body(&resolved, Some(&name), &registry, None) {
        Some(typed) => typed,
        // Not on disk, but perhaps one of the run's artifacts: a file a model
        // made lives in the blob store and was never written to the workdir,
        // and the result route names it by exactly this path.
        None => {
            let bytes = artifact_by_path(id, &meta.workdir, path).ok_or_else(|| {
                ServeError::NotFound(format!(
                    "file '{path}' not found in the run's working directory"
                ))
            })?;
            let mime_type = registry.resolve(None, Some(&name), &bytes);
            (Payload::Loaded(bytes), mime_type)
        }
    };
    Ok(Typed {
        body,
        mime_type,
        name,
    })
}

/// The bytes of the run's artifact recorded at `path`, when it has one and
/// they can still be read (the store by hash, else the workdir).
fn artifact_by_path(run_id: &str, workdir: &str, path: &str) -> Option<Vec<u8>> {
    let output = runstate::read_final_output(run_id)?;
    let artifact = output.artifacts.iter().find(|a| a.path == path)?;
    crate::commands::result::export::artifact_bytes(run_id, workdir, artifact).ok()
}

/// `GET /api/runs/{id}/artifacts/{name}`: the bytes of one file the run
/// handed back, by the name the result route lists it under.
///
/// The one route that follows an artifact the way the runtime does: the
/// store by hash first, so a file a model made and nothing wrote to disk is
/// served, then the workdir.
pub(super) async fn artifact(
    AxumPath((id, name)): AxumPath<(String, String)>,
    Query(query): Query<BytesQuery>,
    headers: HeaderMap,
) -> Result<Response, ServeError> {
    // The record, the answer and the artifact's bytes are read on the
    // blocking pool.
    super::blocking::blocking(move || artifact_named(&id, &name))
        .await?
        .respond(query.download, &headers)
}

/// The bytes of the artifact run `id` handed back as `name`, under the type
/// the answer records and the file name its path ends in.
fn artifact_named(id: &str, name: &str) -> Result<Typed, ServeError> {
    let meta = runstate::read_meta(id).map_err(|_| ServeError::run_not_found(id))?;
    let output = runstate::read_final_output(id)
        .ok_or_else(|| ServeError::NotFound(format!("run '{id}' has not handed back a result")))?;
    let artifact = output
        .artifacts
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| {
            ServeError::NotFound(format!("run '{id}' handed back no artifact named '{name}'"))
        })?;
    let bytes = crate::commands::result::export::artifact_bytes(id, &meta.workdir, artifact)
        .map_err(|e| ServeError::NotFound(e.to_string()))?;
    let file_name = artifact
        .path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(&artifact.name)
        .to_string();
    Ok(Typed {
        body: Payload::Loaded(bytes),
        mime_type: artifact.mime_type.clone(),
        name: file_name,
    })
}

/// One row of the effective mime registry, resolved: what `image/png`
/// inherits from `image/*` is filled in, as `lev mime list` shows it, and a
/// pattern row reports what a type under it inherits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct MimeTypeEntry {
    /// The row's key: a type, or a pattern such as `image/*`.
    pub(super) mime_type: String,
    /// Where the row came from: `builtin`, `config`, or a blueprint or
    /// provider name.
    pub(super) source: String,
    /// The family the type resolves to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) family: Option<String>,
    /// Whether the bytes are text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<bool>,
    /// How the bytes are counted in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tokens: Option<TokenRule>,
    /// The extensions the type is known by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) extensions: Option<Vec<String>>,
    /// The stand-in template a row set, when one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stand_in: Option<String>,
    /// The check script the bytes must pass, when one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) check: Option<String>,
    /// The hex prefix this row identifies its bytes by, from the row itself
    /// rather than from what the type inherits: a magic number belongs to one
    /// key, and a pattern row's prefix is not its family's.
    ///
    /// Skipped on the wire. `GET /api/mime` is the effective registry as `lev
    /// mime list` shows it, and a client reading it byte for byte has never
    /// been sent this. GraphQL reads it off this struct instead.
    #[serde(skip)]
    pub(super) magic: Option<String>,
}

/// The registry listing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct MimeListing {
    /// Every row, keys sorted.
    pub(super) types: Vec<MimeTypeEntry>,
}

/// The rows of `registry`, keys sorted, each resolved through the broader
/// rows it inherits from. Every key the registry holds parses as a type or a
/// pattern, so nothing is left out.
pub(super) fn registry_rows(registry: &MimeRegistry) -> Vec<MimeTypeEntry> {
    let mut keys = registry.keys();
    keys.sort();
    keys.into_iter()
        .filter_map(|(key, source)| {
            MimeType::parse(&key).ok().map(|mime_type| {
                let info = registry.info(&mime_type);
                let magic = registry.row(&key).and_then(|row| row.magic.clone());
                MimeTypeEntry {
                    mime_type: key,
                    source,
                    magic,
                    family: Some(info.family),
                    text: Some(info.text),
                    tokens: Some(info.tokens),
                    extensions: Some(info.extensions),
                    stand_in: info.stand_in,
                    check: info.check,
                }
            })
        })
        .collect()
}

/// `GET /api/mime`: the effective mime registry.
pub(super) async fn list_mime(State(state): State<AppState>) -> Json<MimeListing> {
    // Building the registry reads the providers' scripts and the operator's
    // file, and compiles every check a row names.
    let types = super::blocking::blocking(move || mime_rows(&state)).await;
    Json(MimeListing { types })
}

/// The operator's registry rows, before any blueprint's own. Both surfaces
/// read them here.
pub(super) fn mime_rows(state: &AppState) -> Vec<MimeTypeEntry> {
    registry_rows(&state.config.check().config.mime_registry_or_defaults())
}

/// One row of the effective registry, by its key.
///
/// Total rather than a search that can miss: every caller has just written the
/// row it is asking about, so the registry carries the key. A key it does not
/// carry reads back as the key on its own, which says the same thing an empty
/// registry says about it and is something a client can render.
pub(super) fn mime_row_named(state: &AppState, mime_type: &str) -> MimeTypeEntry {
    mime_rows(state)
        .into_iter()
        .find(|row| row.mime_type == mime_type)
        .unwrap_or(MimeTypeEntry {
            mime_type: mime_type.to_string(),
            source: crate::config::MIME_TYPES_FILE.to_string(),
            family: None,
            text: None,
            tokens: None,
            extensions: None,
            stand_in: None,
            check: None,
            magic: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::serve::testutil::{fixed_config, no_daemon_client};
    use crate::config::Config;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use leviath_core::mime::{Blob, BlobStore, Part};
    use leviath_core::region::EntryContent;
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta};
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    fn state() -> AppState {
        let (tx, _) = broadcast::channel(4);
        AppState {
            caches: Default::default(),
            signer: Default::default(),
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: fixed_config(Config::default()),
            event_tx: tx,
            control: no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        }
    }

    fn app() -> Router {
        Router::new()
            .route("/api/runs/{id}/blobs", get(list_blobs))
            .route("/api/runs/{id}/blobs/{sha256}", get(get_blob))
            .route("/api/runs/{id}/files/raw", get(raw_file))
            .route("/api/runs/{id}/artifacts/{name}", get(artifact))
            .route("/api/mime", get(list_mime))
            .with_state(state())
    }

    async fn call(uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body.to_vec())
    }

    fn seed_run(run_id: &str, workdir: &std::path::Path) -> (String, String) {
        let meta = RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            workdir.to_string_lossy().to_string(),
            1,
        );
        runstate::create_run(&meta).unwrap();
        let registry = MimeRegistry::builtin();
        let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
        let png = Blob::new(
            MimeType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nhero".to_vec(),
        )
        .named("hero.png");
        let stored = Part::stored(store.put(run_id, &png, &registry).unwrap()).named("hero.png");
        let sha = stored.blob().unwrap().sha256.clone();
        // A part the context names but the store lacks.
        let lost = Part::stored(leviath_core::mime::BlobRef {
            sha256: "e".repeat(64),
            mime_type: MimeType::parse("audio/wav").unwrap(),
            size: 3,
            width: None,
            height: None,
            duration_ms: Some(10),
            tokens: 1,
            stand_in: "[audio/wav] lost.wav".to_string(),
        })
        .named("lost.wav");
        let entry = |content: EntryContent| RegionEntrySnapshot {
            content,
            tokens: 1,
            kind: Default::default(),
            metadata: None,
            key: None,
            taint: leviath_core::taint::TaintLevel::Public,
            reasoning: None,
        };
        let region = |name: &str, entries: Vec<RegionEntrySnapshot>| RegionSnapshot {
            name: name.to_string(),
            kind: "pinned".to_string(),
            current_tokens: 1,
            max_tokens: 100,
            entries,
            description: None,
        };
        let snapshot = ContextSnapshot {
            stage_name: "s".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![
                region(
                    "task",
                    // Unnamed here; the copy in `art` names it, and the
                    // listing takes the first name it meets.
                    vec![entry(EntryContent::from_parts(vec![
                        Part::text("see"),
                        Part::stored(stored.blob().expect("stored").clone()),
                    ]))],
                ),
                region(
                    "art",
                    vec![
                        entry(EntryContent::from_parts(vec![stored.clone()])),
                        // The same part again: one listing row, one region.
                        entry(EntryContent::from_parts(vec![stored.clone()])),
                        entry(EntryContent::from_parts(vec![lost])),
                    ],
                ),
            ],
        };
        runstate::write_context_snapshot(run_id, &snapshot).unwrap();
        (sha, "e".repeat(64))
    }

    /// An entry carrying `part`, for a run's state.
    fn holding(
        part: leviath_runtime::state::context::PartState,
    ) -> leviath_runtime::state::EntryState {
        leviath_runtime::state::EntryState {
            text: String::new(),
            parts: vec![part],
            tokens: 1,
            timestamp: 0,
            kind: leviath_runtime::state::EntryKind::Text,
            meta: leviath_runtime::state::EntryMeta::None,
            key: None,
            reasoning: None,
        }
    }

    /// A stored part of `mime_type`, hashed `sha256`, `size` bytes, maybe named.
    fn stored_part(
        sha256: &str,
        mime_type: &str,
        size: u64,
        name: Option<&str>,
    ) -> leviath_runtime::state::context::PartState {
        use leviath_runtime::state::context::{BlobState, PartBody, PartState};
        PartState {
            mime_type: mime_type.to_string(),
            body: PartBody::Stored(BlobState {
                digest: leviath_runtime::spec::names::Digest::new(sha256).unwrap(),
                size,
                width: None,
                height: None,
                duration_ms: None,
                tokens: 1,
                stand_in: format!("[{mime_type}]"),
            }),
            name: name.map(str::to_string),
            deliver: None,
        }
    }

    #[tokio::test]
    async fn blobs_are_listed_and_served_with_their_type() {
        crate::runstate::with_isolated_runs_dir_async("blobs_listed_and_served", |_d| async move {
            use crate::commands::serve::core::run_file::tests::{recorded, step};
            let run_id = recorded();
            let run_id = run_id.as_str();
            let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
            let png = Blob::new(
                MimeType::parse("image/png").unwrap(),
                b"\x89PNG\r\n\x1a\nhero".to_vec(),
            )
            .named("hero.png");
            let sha = store
                .put(run_id, &png, &MimeRegistry::builtin())
                .unwrap()
                .sha256;
            let lost = "e".repeat(64);
            step(run_id, 10, Vec::new(), |s| {
                let regions = &mut s.context.regions;
                let at = |name: &str| regions.iter().position(|r| r.name.as_str() == name);
                let (task, conversation) = (at("task").unwrap(), at("conversation").unwrap());
                // Unnamed here; the copy in the conversation names it, and
                // the listing takes the first name it meets.
                regions[task]
                    .entries
                    .push(holding(stored_part(&sha, "image/png", 12, None)));
                let named = stored_part(&sha, "image/png", 12, Some("hero.png"));
                // The same part twice: one listing row, one region.
                regions[conversation].entries.push(holding(named.clone()));
                regions[conversation].entries.push(holding(named));
                regions[conversation].entries.push(holding(stored_part(
                    &lost,
                    "audio/wav",
                    3,
                    Some("lost.wav"),
                )));
            });

            let (status, _, body) = call(&format!("/api/runs/{run_id}/blobs")).await;
            assert_eq!(status, StatusCode::OK);
            let listing: BlobListing = serde_json::from_slice(&body).unwrap();
            assert_eq!(listing.items.len(), 2);
            assert_eq!(listing.items[0].sha256, sha);
            assert_eq!(listing.items[0].name.as_deref(), Some("hero.png"));
            assert_eq!(listing.items[0].regions, ["task", "conversation"]);
            assert!(listing.items[0].stored);
            assert_eq!(listing.items[1].name.as_deref(), Some("lost.wav"));
            assert!(!listing.items[1].stored);

            let (status, headers, body) = call(&format!("/api/runs/{run_id}/blobs/{sha}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
            assert!(headers.get(header::CONTENT_DISPOSITION).is_none());
            assert_eq!(body, b"\x89PNG\r\n\x1a\nhero");
            // A Range header seeks into the blob: 206 with the slice and a
            // Content-Range naming the whole.
            let req = Request::builder()
                .uri(format!("/api/runs/{run_id}/blobs/{sha}"))
                .header(header::RANGE, "bytes=1-3")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 1-3/12");
            let sliced = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&sliced[..], b"PNG");
            let (_, headers, _) = call(&format!("/api/runs/{run_id}/blobs/{sha}?download=1")).await;
            assert_eq!(
                headers[header::CONTENT_DISPOSITION],
                "attachment; filename=\"hero.png\""
            );

            let (status, _, _) = call(&format!("/api/runs/{run_id}/blobs/{lost}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call(&format!("/api/runs/{run_id}/blobs/zz")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _, _) = call(&format!("/api/runs/ghost/blobs/{sha}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call("/api/runs/ghost/blobs").await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            // A blob on disk that the context no longer names is typed by
            // sniffing and named by its hash.
            let orphan = Blob::new(
                MimeType::parse("image/png").unwrap(),
                b"\x89PNG\r\n\x1a\norphan".to_vec(),
            );
            let orphan_sha = store
                .put(run_id, &orphan, &MimeRegistry::builtin())
                .unwrap()
                .sha256;
            let (status, headers, _) = call(&format!(
                "/api/runs/{run_id}/blobs/{orphan_sha}?download=true"
            ))
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            // Named the way `lev blobs --out` writes it: the short hash with
            // the extension its type gives, so a browser saves a typed file.
            let disposition = headers[header::CONTENT_DISPOSITION].to_str().unwrap();
            let expected = format!("{}.png", orphan_sha.chars().take(12).collect::<String>());
            assert!(disposition.contains(&expected), "{disposition}");
            // A part whose path does not read as a file is an error, not a
            // miss.
            let half = leviath_runtime::spec::names::Digest::of(b"half a part");
            std::fs::create_dir_all(crate::blobs::blob_path(run_id, half.as_str())).unwrap();
            let (status, _, _) = call(&format!("/api/runs/{run_id}/blobs/{half}")).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            let file = runstate::run_dir(run_id).join(leviath_core::files::RUN_FILE);
            // A run with no record answers nothing about its parts.
            std::fs::remove_file(&file).unwrap();
            let (status, _, _) = call(&format!("/api/runs/{run_id}/blobs")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// A run whose record reads and whose run file does not has its listing
    /// refused as the server's fault, not as a run with no parts.
    #[tokio::test]
    async fn a_run_file_that_will_not_read_fails_the_listing() {
        crate::runstate::with_isolated_runs_dir_async("blobs_unreadable", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            seed_run("blobs-broken", workdir.path());
            std::fs::write(
                runstate::run_dir("blobs-broken").join(leviath_core::files::RUN_FILE),
                b"not a run file",
            )
            .unwrap();
            let (status, _, _) = call("/api/runs/blobs-broken/blobs").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        })
        .await;
    }

    #[tokio::test]
    async fn raw_files_are_served_typed_and_fenced() {
        crate::runstate::with_isolated_runs_dir_async("raw_files_served", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(workdir.path().join("out")).unwrap();
            std::fs::write(
                workdir.path().join("out/cut.mp4"),
                b"\x00\x00\x00\x18ftypmp42",
            )
            .unwrap();
            std::fs::write(workdir.path().join("notes.md"), "# n").unwrap();
            let run_id = "raw-run";
            seed_run(run_id, workdir.path());

            let (status, headers, body) =
                call(&format!("/api/runs/{run_id}/files/raw?path=out/cut.mp4")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "video/mp4");
            assert_eq!(body.len(), 12);
            let (_, headers, _) = call(&format!(
                "/api/runs/{run_id}/files/raw?path=notes.md&download=1"
            ))
            .await;
            assert_eq!(headers[header::CONTENT_TYPE], "text/markdown");
            assert_eq!(
                headers[header::CONTENT_DISPOSITION],
                "attachment; filename=\"notes.md\""
            );
            let abs = workdir.path().join("notes.md");
            let (status, _, _) = call(&format!(
                "/api/runs/{run_id}/files/raw?path={}",
                abs.to_string_lossy()
            ))
            .await;
            assert_eq!(status, StatusCode::OK);

            let (status, _, _) =
                call(&format!("/api/runs/{run_id}/files/raw?path=../etc/passwd")).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            let (status, _, _) = call(&format!("/api/runs/{run_id}/files/raw?path=out")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _, _) =
                call(&format!("/api/runs/{run_id}/files/raw?path=missing.bin")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call("/api/runs/ghost/files/raw?path=x").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// An artifact is served by the name the answer lists it under, from the
    /// store by hash when the working directory has no copy, and `files/raw`
    /// falls back to the same store for a path the answer lists.
    #[tokio::test]
    async fn artifacts_are_served_by_name_from_the_store_or_the_workdir() {
        crate::runstate::with_isolated_runs_dir_async("artifacts_served", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("notes.md"), "# written").unwrap();
            let run_id = "artifact-run";
            let mut meta = RunMeta::new(
                run_id.to_string(),
                "agent".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                workdir.path().to_string_lossy().to_string(),
                1,
            );
            let registry = MimeRegistry::builtin();
            let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
            let mesh = Blob::new(
                MimeType::parse("model/gltf-binary").unwrap(),
                b"glTF-bytes".to_vec(),
            );
            let stored = store.put(run_id, &mesh, &registry).unwrap();
            // A mesh a model made (store only), a note the run wrote (workdir
            // only, no hash), and a file in neither place.
            let mut on_disk = leviath_core::output::Artifact::from_path("notes.md");
            on_disk.mime_type = MimeType::parse("text/markdown").unwrap();
            let mut gone = leviath_core::output::Artifact::from_path("out/gone.bin");
            gone.sha256 = "f".repeat(64);
            let output =
                leviath_core::output::FinalOutput::new("built", None, "build".to_string(), 0)
                    .with_artifacts(vec![
                        leviath_core::output::Artifact {
                            name: "mesh".to_string(),
                            path: "out/scene.glb".to_string(),
                            mime_type: MimeType::parse("model/gltf-binary").unwrap(),
                            size: 10,
                            sha256: stored.sha256.clone(),
                        },
                        on_disk,
                        gone,
                    ]);
            meta.final_output = Some(output.descriptor());
            runstate::create_run(&meta).unwrap();
            runstate::write_final_output(&runstate::run_dir(run_id), &output.content).unwrap();

            let (status, headers, body) = call(&format!("/api/runs/{run_id}/artifacts/mesh")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "model/gltf-binary");
            assert_eq!(body, b"glTF-bytes");
            let (_, headers, _) =
                call(&format!("/api/runs/{run_id}/artifacts/mesh?download=1")).await;
            assert_eq!(
                headers[header::CONTENT_DISPOSITION],
                "attachment; filename=\"scene.glb\""
            );
            let (status, _, body) = call(&format!("/api/runs/{run_id}/artifacts/notes.md")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, b"# written");
            let (status, _, _) = call(&format!("/api/runs/{run_id}/artifacts/gone.bin")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call(&format!("/api/runs/{run_id}/artifacts/nope")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call("/api/runs/ghost/artifacts/mesh").await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            // The raw route reaches the stored mesh by the path the answer
            // lists, and still 404s a path that is neither on disk nor listed.
            let (status, headers, body) =
                call(&format!("/api/runs/{run_id}/files/raw?path=out/scene.glb")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "model/gltf-binary");
            assert_eq!(body, b"glTF-bytes");
            let (status, _, _) =
                call(&format!("/api/runs/{run_id}/files/raw?path=out/gone.bin")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) =
                call(&format!("/api/runs/{run_id}/files/raw?path=nowhere.bin")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            // A run that has not answered has no artifacts to serve.
            let silent = "silent-run";
            seed_run(silent, workdir.path());
            let (status, _, _) = call(&format!("/api/runs/{silent}/artifacts/mesh")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// `GET /api/mime` carries exactly the keys it always has.
    ///
    /// `MimeTypeEntry` grew a `magic` field for GraphQL to read. This route is
    /// the effective registry as `lev mime list` shows it, and a client
    /// reading it key by key has never been sent one, so the field is skipped
    /// on the wire and this is what holds it there.
    #[tokio::test]
    async fn the_registry_listing_carries_no_new_keys() {
        let (_, _, body) = call("/api/mime").await;
        let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let png = listing["types"]
            .as_array()
            .expect("the rows")
            .iter()
            .find(|row| row["mime_type"] == "image/png")
            .expect("the built-in rows are there");
        // Sorted, because the JSON is read back through a map that sorts: what
        // is being held here is the set of keys, not their order on the wire.
        let keys: Vec<&String> = png.as_object().expect("an object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "extensions",
                "family",
                "mime_type",
                "source",
                "text",
                "tokens"
            ],
            "the row's JSON is what it has always been"
        );
    }

    #[tokio::test]
    async fn the_registry_is_listed_with_sources() {
        let (status, _, body) = call("/api/mime").await;
        assert_eq!(status, StatusCode::OK);
        let listing: MimeListing = serde_json::from_slice(&body).unwrap();
        let png = listing
            .types
            .iter()
            .find(|t| t.mime_type == "image/png")
            .expect("the built-in rows are there");
        assert_eq!(png.source, "builtin");
        assert_eq!(png.extensions.as_deref(), Some(&["png".to_string()][..]));
        // Resolved, as `lev mime list` shows it: the family and the token rule
        // come from the `image/*` row the type inherits from.
        assert_eq!(png.family.as_deref(), Some("image"));
        assert_eq!(png.text, Some(false));
        assert!(png.tokens.is_some());
        let pattern = listing
            .types
            .iter()
            .find(|t| t.mime_type == "image/*")
            .expect("pattern rows are listed as written");
        assert_eq!(pattern.family.as_deref(), Some("image"));
        assert!(
            pattern
                .extensions
                .as_deref()
                .is_none_or(<[String]>::is_empty)
        );
        assert!(listing.types.iter().any(|t| t.mime_type == "*/*"));
        let names: Vec<&str> = listing.types.iter().map(|t| t.mime_type.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn a_flag_reads_the_ways_it_is_spelled() {
        let read = |json: &str| serde_json::from_str::<BytesQuery>(json).unwrap().download;
        assert!(read("{\"download\":\"1\"}"));
        assert!(read("{\"download\":\" TRUE \"}"));
        assert!(!read("{\"download\":\"0\"}"));
        assert!(!read("{}"));
        assert!(serde_json::from_str::<BytesQuery>("{\"download\":1}").is_err());
    }

    #[test]
    fn resolve_range_reads_every_shape() {
        use RangeResult::*;
        // No header, or an unknown unit, or a list: serve the whole body.
        assert_eq!(resolve_range(None, 10), Full);
        assert_eq!(resolve_range(Some("lines=0-1"), 10), Full);
        assert_eq!(resolve_range(Some("bytes=0-1,4-5"), 10), Full);
        assert_eq!(resolve_range(Some("bytes=abc"), 10), Full);
        assert_eq!(resolve_range(Some("bytes=x-1"), 10), Full);
        assert_eq!(resolve_range(Some("bytes=0-z"), 10), Full);
        assert_eq!(resolve_range(Some("bytes=-z"), 10), Full);
        // A closed range, an open end (clamped), and a suffix.
        assert_eq!(resolve_range(Some("bytes=2-5"), 10), Partial(2, 5));
        assert_eq!(resolve_range(Some("bytes=2-"), 10), Partial(2, 9));
        assert_eq!(resolve_range(Some("bytes=0-100"), 10), Partial(0, 9));
        assert_eq!(resolve_range(Some("bytes=-3"), 10), Partial(7, 9));
        assert_eq!(resolve_range(Some("bytes=-100"), 10), Partial(0, 9));
        // Past the end, reversed, a zero-length suffix, and an empty body.
        assert_eq!(resolve_range(Some("bytes=10-12"), 10), Unsatisfiable);
        assert_eq!(resolve_range(Some("bytes=5-2"), 10), Unsatisfiable);
        assert_eq!(resolve_range(Some("bytes=-0"), 10), Unsatisfiable);
        assert_eq!(resolve_range(Some("bytes=0-0"), 0), Unsatisfiable);
    }

    #[test]
    fn a_partial_and_an_unsatisfiable_range_shape_the_response() {
        let png = MimeType::parse("image/png").unwrap();
        // 206 with the slice, its Content-Range, and Accept-Ranges.
        let r = bytes_response(
            Payload::Loaded(b"abcdef".to_vec()),
            &png,
            "x.png",
            false,
            Some("bytes=1-3"),
        )
        .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
        assert_eq!(r.headers()[header::ACCEPT_RANGES], "bytes");
        // 416 for a range past the end, with `*/total`.
        let r = bytes_response(
            Payload::Loaded(b"abcdef".to_vec()),
            &png,
            "x.png",
            false,
            Some("bytes=9-10"),
        )
        .unwrap();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes */6");
    }

    /// A file checked for UTF-8 a block at a time reads as it would whole: a
    /// character split across a block boundary is still one character, a
    /// bad byte past the first block is still found, and a character cut
    /// off at the end is not valid.
    #[test]
    fn a_file_is_checked_for_utf8_across_its_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let check = |bytes: &[u8]| {
            let path = dir.path().join("f");
            std::fs::write(&path, bytes).unwrap();
            is_utf8(&std::fs::File::open(&path).unwrap())
        };
        let mut split = vec![b'a'; BLOCK - 1];
        split.extend("é and more".as_bytes());
        assert!(check(&split));
        let mut bad = vec![b'a'; BLOCK + 10];
        bad.push(0xff);
        assert!(!check(&bad));
        let mut cut = vec![b'a'; 10];
        cut.push(0xc3);
        assert!(!check(&cut));
        assert!(check(b""));
    }

    /// A small file is read whole and typed from all of it; a bigger one is
    /// left on disk and typed from its head, then its name, then by reading
    /// it through for UTF-8.
    #[test]
    fn a_file_is_read_whole_only_when_small() {
        let dir = tempfile::tempdir().unwrap();
        let registry = MimeRegistry::builtin();
        let typed = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let (body, mime_type) = file_body(&path, Some(name), &registry, None).unwrap();
            let loaded = matches!(body, Payload::Loaded(_));
            (loaded, mime_type.as_str().to_string())
        };
        assert_eq!(typed("small.txt", b"hello"), (true, "text/plain".into()));
        let big_text = vec![b'a'; (LOADED_BYTES + 1) as usize];
        assert_eq!(typed("big", &big_text), (false, "text/plain".into()));
        let mut big_binary = big_text.clone();
        big_binary.push(0xff);
        assert_eq!(
            typed("big-binary", &big_binary),
            (false, "application/octet-stream".into())
        );
        let mut big_png = b"\x89PNG\r\n\x1a\n".to_vec();
        big_png.extend(&big_binary);
        assert_eq!(typed("art", &big_png), (false, "image/png".into()));
        let known = MimeType::parse("video/mp4").unwrap();
        let path = dir.path().join("big");
        let (_, kept) = file_body(&path, None, &registry, Some(known.clone())).unwrap();
        assert_eq!(kept, known);
        assert!(file_body(&dir.path().join("gone"), None, &registry, None).is_none());
    }

    /// A range of a big file on disk is read for the window asked for: one
    /// byte from deep inside a sparse file comes back as that byte.
    #[tokio::test]
    async fn a_range_of_a_file_on_disk_reads_only_its_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.bin");
        let file = std::fs::File::create(&path).unwrap();
        let len: u64 = 256 << 20;
        file.set_len(len).unwrap();
        {
            use std::io::{Seek as _, Write as _};
            let mut file = &file;
            file.seek(std::io::SeekFrom::Start(len - 10)).unwrap();
            file.write_all(b"Z").unwrap();
        }
        let octets = MimeType::parse("application/octet-stream").unwrap();
        let open = || Payload::File(std::fs::File::open(&path).unwrap(), len);
        let range = format!("bytes={}-{}", len - 10, len - 10);
        let r = bytes_response(open(), &octets, "s.bin", false, Some(&range)).unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(r.into_body(), 16).await.unwrap();
        assert_eq!(&body[..], b"Z");
        let whole = bytes_response(open(), &octets, "s.bin", false, Some("bytes=0-3")).unwrap();
        let body = axum::body::to_bytes(whole.into_body(), 16).await.unwrap();
        assert_eq!(&body[..], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_name_the_header_cannot_carry_is_made_safe() {
        let response = bytes_response(
            Payload::Loaded(vec![1]),
            &MimeType::parse("image/png").unwrap(),
            "we ird/na\"me.png",
            true,
            None,
        )
        .unwrap();
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"we_ird_na_me.png\""
        );
    }
}
