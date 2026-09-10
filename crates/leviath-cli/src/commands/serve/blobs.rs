//! A run's stored parts, its raw files, and the mime registry, over HTTP.
//!
//! `GET /api/agents/{id}/blobs` lists every stored part the run's context
//! holds, by hash, with what the context says about it: name, type, size,
//! dimensions, and the regions carrying it. `GET .../blobs/{sha256}` serves
//! the bytes under their own `Content-Type`, and `?download=1` asks the
//! browser to save rather than show. `GET .../files/raw?path=` serves a
//! workdir file the same way, typed by the registry. `GET /api/mime` is
//! the effective registry: every row and where it came from.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use leviath_core::mime::{MimeRegistry, MimeType, is_sha256_hex};
use serde::{Deserialize, Serialize};

use super::types::*;
use crate::blobs::{BlobEntry, blob_path};
use crate::runstate;

/// The stored parts a run's context holds, or 404 when it has no context.
fn stored_parts(run_id: &str) -> Result<Vec<BlobEntry>, ApiError> {
    crate::blobs::list(run_id).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("No context snapshot for run '{run_id}'"),
        )
    })
}

/// Every stored part a run holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct BlobListing {
    /// The parts, first appearance first.
    pub(super) items: Vec<BlobEntry>,
}

/// `GET /api/agents/{id}/blobs`: every stored part the run holds.
pub(super) async fn list_blobs(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<BlobListing>, ApiError> {
    runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Agent run '{id}' not found")))?;
    Ok(Json(BlobListing {
        items: stored_parts(&id)?,
    }))
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

/// Bytes with their type, and a download hint when asked for.
fn bytes_response(
    bytes: Vec<u8>,
    mime_type: &MimeType,
    name: &str,
    download: bool,
) -> Result<Response, ApiError> {
    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime_type.as_str()).expect("a mime type is printable ASCII"),
    );
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

/// `GET /api/agents/{id}/blobs/{sha256}`: the bytes of one stored part.
pub(super) async fn get_blob(
    State(state): State<AppState>,
    AxumPath((id, sha256)): AxumPath<(String, String)>,
    Query(query): Query<BytesQuery>,
) -> Result<Response, ApiError> {
    if !is_sha256_hex(&sha256) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "a blob is named by its 64-character lowercase hex sha256".to_string(),
        ));
    }
    runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Agent run '{id}' not found")))?;
    let bytes = tokio::fs::read(blob_path(&id, &sha256))
        .await
        .map_err(|_| {
            err(
                StatusCode::NOT_FOUND,
                format!("run '{id}' holds no blob {sha256}"),
            )
        })?;
    // What the context says about it, when it says anything: the type it
    // was stored as and the name it carries. A blob the context no longer
    // references is typed by sniffing, and named by its hash.
    let known = stored_parts(&id)
        .unwrap_or_default()
        .into_iter()
        .find(|b| b.sha256 == sha256);
    let registry = state.current_config().mime_registry_or_defaults();
    let mime_type = known
        .as_ref()
        .and_then(|b| MimeType::parse(&b.mime_type).ok())
        .unwrap_or_else(|| registry.resolve(None, None, &bytes));
    let name = known
        .and_then(|b| b.name)
        .unwrap_or_else(|| sha256.chars().take(12).collect());
    bytes_response(bytes, &mime_type, &name, query.download)
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

/// `GET /api/agents/{id}/files/raw?path=`: a workdir file's bytes, typed.
pub(super) async fn raw_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<RawFileQuery>,
) -> Result<Response, ApiError> {
    let meta = runstate::read_meta(&id)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("Agent run '{id}' not found")))?;
    let workdir = PathBuf::from(&meta.workdir);
    let requested = PathBuf::from(&query.path);
    let resolved = match requested.is_absolute() {
        true => requested,
        false => workdir.join(&requested),
    };
    if !leviath_core::resolves_within(&resolved, &workdir) {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!(
                "path '{}' is outside the run's working directory",
                query.path
            ),
        ));
    }
    if resolved.is_dir() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "'{}' is a directory; the raw route serves files",
                query.path
            ),
        ));
    }
    let bytes = tokio::fs::read(&resolved).await.map_err(|_| {
        err(
            StatusCode::NOT_FOUND,
            format!(
                "file '{}' not found in the run's working directory",
                query.path
            ),
        )
    })?;
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let registry = state.current_config().mime_registry_or_defaults();
    let mime_type = registry.resolve(None, Some(&name), &bytes);
    bytes_response(bytes, &mime_type, &name, query.download)
}

/// One row of the effective mime registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MimeTypeEntry {
    /// The row's key: a type, or a pattern such as `image/*`.
    pub(super) mime_type: String,
    /// Where the row came from: `builtin`, `config`, or a blueprint or
    /// provider name.
    pub(super) source: String,
    /// The family the row sets, when it sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) family: Option<String>,
    /// Whether the row calls the bytes text, when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<bool>,
    /// The extensions the row names, when it names any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) extensions: Option<Vec<String>>,
}

/// The registry listing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MimeListing {
    /// Every row, keys sorted.
    pub(super) types: Vec<MimeTypeEntry>,
}

/// The rows of `registry`, keys sorted.
pub(super) fn registry_rows(registry: &MimeRegistry) -> Vec<MimeTypeEntry> {
    let mut keys = registry.keys();
    keys.sort();
    keys.into_iter()
        .map(|(key, source)| {
            let row = registry.row(&key).cloned().unwrap_or_default();
            MimeTypeEntry {
                mime_type: key,
                source,
                family: row.family,
                text: row.text,
                extensions: row.extensions,
            }
        })
        .collect()
}

/// `GET /api/mime`: the effective mime registry.
pub(super) async fn list_mime(State(state): State<AppState>) -> Json<MimeListing> {
    let registry = state.current_config().mime_registry_or_defaults();
    Json(MimeListing {
        types: registry_rows(&registry),
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
            .route("/api/agents/{id}/blobs", get(list_blobs))
            .route("/api/agents/{id}/blobs/{sha256}", get(get_blob))
            .route("/api/agents/{id}/files/raw", get(raw_file))
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

    #[tokio::test]
    async fn blobs_are_listed_and_served_with_their_type() {
        crate::runstate::with_isolated_runs_dir_async("blobs_listed_and_served", |_d| async move {
            let workdir = tempfile::tempdir().unwrap();
            let run_id = "blobs-run";
            let (sha, lost) = seed_run(run_id, workdir.path());

            let (status, _, body) = call(&format!("/api/agents/{run_id}/blobs")).await;
            assert_eq!(status, StatusCode::OK);
            let listing: BlobListing = serde_json::from_slice(&body).unwrap();
            assert_eq!(listing.items.len(), 2);
            assert_eq!(listing.items[0].sha256, sha);
            assert_eq!(listing.items[0].name.as_deref(), Some("hero.png"));
            assert_eq!(listing.items[0].regions, ["task", "art"]);
            assert!(listing.items[0].stored);
            assert_eq!(listing.items[1].name.as_deref(), Some("lost.wav"));
            assert!(!listing.items[1].stored);

            let (status, headers, body) = call(&format!("/api/agents/{run_id}/blobs/{sha}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            assert!(headers.get(header::CONTENT_DISPOSITION).is_none());
            assert_eq!(body, b"\x89PNG\r\n\x1a\nhero");
            let (_, headers, _) =
                call(&format!("/api/agents/{run_id}/blobs/{sha}?download=1")).await;
            assert_eq!(
                headers[header::CONTENT_DISPOSITION],
                "attachment; filename=\"hero.png\""
            );

            let (status, _, _) = call(&format!("/api/agents/{run_id}/blobs/{lost}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call(&format!("/api/agents/{run_id}/blobs/zz")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _, _) = call(&format!("/api/agents/ghost/blobs/{sha}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call("/api/agents/ghost/blobs").await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            // A blob on disk that the context no longer names is typed by
            // sniffing and named by its hash.
            let orphan = Blob::new(
                MimeType::parse("image/png").unwrap(),
                b"\x89PNG\r\n\x1a\norphan".to_vec(),
            );
            let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
            let orphan_sha = store
                .put(run_id, &orphan, &MimeRegistry::builtin())
                .unwrap()
                .sha256;
            let (status, headers, _) = call(&format!(
                "/api/agents/{run_id}/blobs/{orphan_sha}?download=true"
            ))
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            assert!(
                headers[header::CONTENT_DISPOSITION]
                    .to_str()
                    .unwrap()
                    .contains(&orphan_sha.chars().take(12).collect::<String>())
            );
            // A run with no snapshot has no listing but still serves a blob.
            std::fs::remove_file(runstate::run_dir(run_id).join(leviath_core::files::CONTEXT_FILE))
                .unwrap();
            let (status, _, _) = call(&format!("/api/agents/{run_id}/blobs")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call(&format!("/api/agents/{run_id}/blobs/{sha}")).await;
            assert_eq!(status, StatusCode::OK);
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
                call(&format!("/api/agents/{run_id}/files/raw?path=out/cut.mp4")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "video/mp4");
            assert_eq!(body.len(), 12);
            let (_, headers, _) = call(&format!(
                "/api/agents/{run_id}/files/raw?path=notes.md&download=1"
            ))
            .await;
            assert_eq!(headers[header::CONTENT_TYPE], "text/markdown");
            assert_eq!(
                headers[header::CONTENT_DISPOSITION],
                "attachment; filename=\"notes.md\""
            );
            let abs = workdir.path().join("notes.md");
            let (status, _, _) = call(&format!(
                "/api/agents/{run_id}/files/raw?path={}",
                abs.to_string_lossy()
            ))
            .await;
            assert_eq!(status, StatusCode::OK);

            let (status, _, _) = call(&format!(
                "/api/agents/{run_id}/files/raw?path=../etc/passwd"
            ))
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            let (status, _, _) = call(&format!("/api/agents/{run_id}/files/raw?path=out")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, _, _) =
                call(&format!("/api/agents/{run_id}/files/raw?path=missing.bin")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call("/api/agents/ghost/files/raw?path=x").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
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
    fn a_name_the_header_cannot_carry_is_made_safe() {
        let response = bytes_response(
            vec![1],
            &MimeType::parse("image/png").unwrap(),
            "we ird/na\"me.png",
            true,
        )
        .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"we_ird_na_me.png\""
        );
    }
}
