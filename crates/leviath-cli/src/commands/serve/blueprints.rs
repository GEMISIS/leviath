//! Blueprint discovery, CRUD, and validation endpoints.

use std::path::{Path, PathBuf};

use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use leviath_core::manifest::parse_manifest;

/// Resolve the installed agents directory.
pub(super) fn agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".leviath")
        .join("agents")
}

/// Scan for blueprints from installed agents dir and configured agent_paths.
pub(super) fn discover_blueprints(config: &crate::config::Config) -> Vec<BlueprintInfo> {
    let mut results = Vec::new();
    let agents = agents_dir();

    let mut dirs_to_scan: Vec<PathBuf> = vec![agents];
    dirs_to_scan.extend(config.agent_paths.iter().cloned());

    for dir in dirs_to_scan {
        if !dir.exists() {
            continue;
        }
        // Check dir itself
        let manifest = dir.join("agent.leviath");
        if manifest.exists() {
            results.extend(read_blueprint_info(&manifest, &dir));
        }
        // Check subdirs
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                let m = p.join("agent.leviath");
                if m.exists() {
                    results.extend(read_blueprint_info(&m, &p));
                }
            }
        }
    }

    results
}

pub(super) fn read_blueprint_info(manifest_path: &Path, dir: &Path) -> Option<BlueprintInfo> {
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let bp = parse_manifest(&content).ok()?;
    Some(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
    })
}

pub(super) async fn list_blueprints(State(state): State<AppState>) -> Json<Vec<BlueprintInfo>> {
    Json(discover_blueprints(&state.config))
}

pub(super) async fn get_blueprint(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<BlueprintInfo>, StatusCode> {
    let blueprints = discover_blueprints(&state.config);
    blueprints
        .into_iter()
        .find(|b| b.name == name)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

pub(super) async fn create_blueprint(
    Json(body): Json<CreateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    // Validate manifest first, keeping the parsed Blueprint so the response
    // can be built from it directly below instead of re-reading the file we
    // just wrote (which used to make the re-read's error arm a TOCTOU-only,
    // untestable dead branch).
    let bp = parse_manifest(&body.manifest).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        )
    })?;

    let dir = agents_dir().join(&body.name);
    std::fs::create_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to create directory: {}", e),
            }),
        )
    })?;

    let manifest_path = dir.join("agent.leviath");
    std::fs::write(&manifest_path, &body.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write manifest: {}", e),
            }),
        )
    })?;

    Ok(Json(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
    }))
}

pub(super) async fn update_blueprint(
    AxumPath(name): AxumPath<String>,
    Json(body): Json<UpdateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    let bp = parse_manifest(&body.manifest).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        )
    })?;

    let dir = agents_dir().join(&name);
    let manifest_path = dir.join("agent.leviath");
    if !manifest_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", name),
            }),
        ));
    }

    std::fs::write(&manifest_path, &body.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to write manifest: {}", e),
            }),
        )
    })?;

    Ok(Json(BlueprintInfo {
        name: bp.name,
        version: bp.version,
        description: bp.description,
        path: dir.to_string_lossy().to_string(),
        stages: bp.stages.iter().map(|s| s.name.clone()).collect(),
    }))
}

pub(super) async fn delete_blueprint(
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let dir = agents_dir().join(&name);
    if !dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blueprint '{}' not found", name),
            }),
        ));
    }

    std::fs::remove_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to delete blueprint: {}", e),
            }),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn validate_blueprint(
    Json(body): Json<ValidateBlueprintReq>,
) -> Json<ValidateResponse> {
    match parse_manifest(&body.manifest) {
        Ok(bp) => match bp.validate() {
            Ok(()) => Json(ValidateResponse {
                valid: true,
                errors: None,
            }),
            Err(e) => Json(ValidateResponse {
                valid: false,
                errors: Some(vec![e.to_string()]),
            }),
        },
        Err(e) => Json(ValidateResponse {
            valid: false,
            errors: Some(vec![e.to_string()]),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::config::Config;

    fn test_state_with_path(path: PathBuf) -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: Arc::new(Config {
                agent_paths: vec![path],
                ..Default::default()
            }),
            event_tx: tx,
        }
    }

    fn test_manifest() -> &'static str {
        r#"
[agent]
name = "test-bp"
version = "1.0.0"
description = "A test blueprint"

[stages.plan]
prompt = "Plan the work"
"#
    }

    // ─── list_blueprints ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_blueprints_empty_path_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let blueprints: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // No blueprints in the empty temp dir (ignoring ~/.leviath/agents)
        let _ = blueprints;
    }

    #[tokio::test]
    async fn list_blueprints_with_agent_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), test_manifest()).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints", get(list_blueprints))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let blueprints: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_test_bp_listed(&blueprints);
    }

    fn assert_test_bp_listed(blueprints: &[serde_json::Value]) {
        assert!(
            blueprints
                .iter()
                .any(|b| b["name"].as_str() == Some("test-bp")),
            "test-bp should be listed"
        );
    }

    #[test]
    #[should_panic(expected = "test-bp should be listed")]
    fn assert_test_bp_listed_panics_when_missing() {
        assert_test_bp_listed(&[]);
    }

    // ─── get_blueprint ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_blueprint_existing_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("test-bp");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), test_manifest()).unwrap();

        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/test-bp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let bp: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(bp["name"].as_str().unwrap(), "test-bp");
        assert_eq!(bp["version"].as_str().unwrap(), "1.0.0");
    }

    #[tokio::test]
    async fn get_blueprint_not_found_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_with_path(dir.path().to_path_buf());
        let app = Router::new()
            .route("/api/blueprints/{name}", get(get_blueprint))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/blueprints/does-not-exist-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// Unique blueprint name so tests operating against the real
    /// `~/.leviath/agents` dir (create/update/delete have no path DI seam)
    /// don't collide with each other or with a developer's real agents.
    fn unique_bp_name(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("test-bp-{}-{}-{}", prefix, std::process::id(), nanos)
    }

    // ─── create_blueprint ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_blueprint_valid_manifest_returns_ok() {
        let name = unique_bp_name("create");
        let manifest = format!(
            r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "Created via API"

[stages.plan]
prompt = "Plan the work"
"#
        );

        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let body = serde_json::json!({ "name": name, "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(info["name"].as_str().unwrap(), name);
        assert_eq!(info["stages"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(agents_dir().join(&name));
    }

    #[tokio::test]
    async fn create_blueprint_dir_creation_failure_returns_500() {
        // Force `create_dir_all` to fail deterministically by pre-creating a
        // regular *file* at the target path — a directory can't be created
        // where a non-directory entry already exists. This is cross-platform:
        // both Unix (ENOTDIR/EEXIST) and Windows (ERROR_ALREADY_EXISTS) refuse
        // to create a directory at a path that's already occupied by a file.
        let name = unique_bp_name("create-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(agents_dir()).unwrap();
        std::fs::write(&dir, b"blocking file").unwrap();

        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let manifest = format!(
            "\n[agent]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n[stages.plan]\nprompt = \"p\"\n"
        );
        let body = serde_json::json!({ "name": name, "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        let _ = std::fs::remove_file(&dir);
    }

    #[tokio::test]
    async fn create_blueprint_invalid_manifest_returns_400() {
        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let body = serde_json::json!({
            "name": "bad-agent",
            "manifest": "not valid toml [[[{"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_blueprint_manifest_write_failure_returns_500() {
        // Distinct from `create_blueprint_dir_creation_failure_returns_500`:
        // here `create_dir_all` succeeds (the blueprint dir doesn't already
        // exist as a blocking file), but the manifest *file* write fails --
        // forced by pre-creating a directory at the exact path
        // `<dir>/agent.leviath`, so `std::fs::write` hits EISDIR.
        let name = unique_bp_name("create-manifest-write-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(dir.join("agent.leviath")).unwrap();

        let app = Router::new().route("/api/blueprints", post(create_blueprint));
        let manifest = format!(
            "\n[agent]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n[stages.plan]\nprompt = \"p\"\n"
        );
        let body = serde_json::json!({ "name": name, "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── update_blueprint ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_blueprint_write_failure_returns_500() {
        use axum::routing::put;

        // Force `std::fs::write` to fail deterministically: the manifest
        // file exists (so the not-found check passes) but is read-only, so
        // overwriting it fails. `set_readonly` is cross-platform (Unix
        // clears/sets the owner-write bit; Windows toggles the FILE_ATTRIBUTE
        // _READONLY flag), and both platforms' `std::fs::write` honor it.
        let name = unique_bp_name("update-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("agent.leviath");
        std::fs::write(&manifest_path, test_manifest()).unwrap();
        let mut perms = std::fs::metadata(&manifest_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&manifest_path, perms).unwrap();

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let body = serde_json::json!({ "manifest": test_manifest() });
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/blueprints/{}", name))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        // Test-only cleanup so the temp dir can be removed afterward -- not
        // production/security-relevant code, so clippy's warning about
        // `set_readonly(false)` making the file world-writable on Unix
        // doesn't apply here.
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut perms = std::fs::metadata(&manifest_path).unwrap().permissions();
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(&manifest_path, perms);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_blueprint_existing_returns_ok() {
        use axum::routing::put;

        let name = unique_bp_name("update");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "Original"

[stages.plan]
prompt = "Plan"
"#
            ),
        )
        .unwrap();

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let updated_manifest = format!(
            r#"
[agent]
name = "{name}"
version = "2.0.0"
description = "Updated"

[stages.plan]
prompt = "Plan"

[stages.implement]
prompt = "Implement"
"#
        );
        let body = serde_json::json!({ "manifest": updated_manifest });
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/blueprints/{}", name))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(info["version"].as_str().unwrap(), "2.0.0");
        assert_eq!(info["stages"].as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_blueprint_invalid_manifest_returns_400() {
        use axum::routing::put;

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let body = serde_json::json!({
            "manifest": "not valid toml {{{"
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/api/blueprints/my-agent")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn update_blueprint_not_found_returns_404() {
        use axum::routing::put;

        let app = Router::new().route("/api/blueprints/{name}", put(update_blueprint));
        let body = serde_json::json!({
            "manifest": r#"
[agent]
name = "no-such-agent"
version = "1.0.0"
description = "Missing"

[stages.run]
prompt = "Run"
"#
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/api/blueprints/no-such-agent-xyz-99999")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── delete_blueprint ─────────────────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_blueprint_removal_failure_returns_500() {
        use axum::routing::delete;
        use std::os::unix::fs::PermissionsExt;

        // Force `remove_dir_all` to fail deterministically: the blueprint
        // dir exists (so the not-found check passes) but is made read-only
        // and non-executable, so unlinking its contents fails with EACCES.
        let name = unique_bp_name("delete-fail");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.leviath"), test_manifest()).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/blueprints/{}", name))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        // Restore perms so cleanup (and any subsequent test) can remove it.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows twin of `delete_blueprint_removal_failure_returns_500`.
    ///
    /// On Unix, directory *write* permission (not the file's own
    /// permission) governs whether an entry can be unlinked from a
    /// directory, so making the directory `0o555` is what forces
    /// `remove_dir_all` to fail there. Windows has no equivalent
    /// "directory write permission" concept via `std::fs::Permissions`, and
    /// -- contrary to what an earlier version of this test assumed --
    /// marking a file inside the directory read-only does NOT make
    /// `remove_dir_all` fail on Windows: it clears the read-only attribute
    /// before deleting, the same way it silently succeeds through other
    /// removable-but-`readonly` obstacles. A real sharing violation does
    /// still block deletion, though: holding an exclusive (no-share) file
    /// handle open on a file inside the directory for the duration of the
    /// request -- the same technique
    /// `session.rs`'s `resolve_task_unreadable_file_returns_error` Windows
    /// twin uses -- reliably makes `remove_dir_all` fail there.
    #[cfg(windows)]
    #[tokio::test]
    async fn delete_blueprint_removal_failure_returns_500_windows() {
        use axum::routing::delete;
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let name = unique_bp_name("delete-fail-win");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("agent.leviath");
        std::fs::write(&manifest_path, test_manifest()).unwrap();

        // Hold an exclusive (no-share) handle open for the duration of the
        // delete attempt below, so `remove_dir_all` hits a sharing
        // violation trying to unlink `manifest_path`.
        let _locked = OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&manifest_path)
            .unwrap();

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/blueprints/{}", name))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        drop(_locked);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_blueprint_existing_returns_no_content() {
        use axum::routing::delete;

        let name = unique_bp_name("delete");
        let dir = agents_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.leviath"), test_manifest()).unwrap();
        assert!(dir.exists());

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/blueprints/{}", name))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NO_CONTENT);
        assert_dir_removed(&dir);
    }

    fn assert_dir_removed(dir: &std::path::Path) {
        assert!(!dir.exists(), "directory should be removed");
    }

    #[test]
    #[should_panic(expected = "directory should be removed")]
    fn assert_dir_removed_panics_when_still_present() {
        assert_dir_removed(std::path::Path::new("."));
    }

    #[tokio::test]
    async fn delete_blueprint_not_found_returns_404() {
        use axum::routing::delete;

        let app = Router::new().route("/api/blueprints/{name}", delete(delete_blueprint));
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/blueprints/nonexistent-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ─── validate_blueprint ───────────────────────────────────────────────────

    #[tokio::test]
    async fn validate_blueprint_valid_manifest_returns_ok_valid_true() {
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let body = serde_json::json!({"manifest": test_manifest()});
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(result.valid);
        assert!(result.errors.is_none());
    }

    #[tokio::test]
    async fn validate_blueprint_invalid_manifest_returns_ok_valid_false() {
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let body = serde_json::json!({"manifest": "not toml at all [[[{"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!result.valid);
        assert!(result.errors.is_some());
    }

    #[tokio::test]
    async fn validate_blueprint_parses_but_fails_structural_validation_returns_ok_valid_false() {
        // Distinct from the manifest above: this one parses fine as TOML/a
        // Blueprint (Ok(bp) from parse_manifest), but bp.validate()
        // itself rejects it -- an entry_stage that doesn't match any defined
        // stage. Exercises the `Ok(bp) => match bp.validate() { Err(e) => .. }`
        // arm, which `validate_blueprint_invalid_manifest_returns_ok_valid_false`
        // (a parse failure) never reaches.
        let app = Router::new().route("/api/blueprints/validate", post(validate_blueprint));
        let manifest = r#"
[agent]
name = "bad-entry-stage"
version = "1.0.0"
description = "Entry stage doesn't exist"
entry_stage = "does-not-exist"

[stages.plan]
prompt = "Plan"
"#;
        let body = serde_json::json!({"manifest": manifest});
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!result.valid);
        assert!(result
            .errors
            .unwrap()
            .iter()
            .any(|e| e.contains("entry_stage")));
    }

    #[test]
    fn agents_dir_is_under_home() {
        let dir = agents_dir();
        let path_str = dir.to_string_lossy();
        assert!(path_str.contains(".leviath"));
        assert!(path_str.ends_with("agents"));
    }

    #[test]
    fn read_blueprint_info_from_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        let content = r#"
[agent]
name = "test-bp"
version = "1.0.0"
description = "A test blueprint"

[stages.plan]
prompt = "Plan the work"
"#;
        std::fs::write(&manifest_path, content).unwrap();

        let info = read_blueprint_info(&manifest_path, dir.path()).unwrap();
        assert_eq!(info.name, "test-bp");
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.description, "A test blueprint");
        assert_eq!(info.stages, vec!["plan"]);
        assert_eq!(info.path, dir.path().to_string_lossy());
    }

    #[test]
    fn read_blueprint_info_nonexistent_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("nonexistent.leviath");
        let result = read_blueprint_info(&manifest_path, dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn read_blueprint_info_invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        std::fs::write(&manifest_path, "not valid toml [[[").unwrap();
        let result = read_blueprint_info(&manifest_path, dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn read_blueprint_info_multiple_stages() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        let content = r#"
[agent]
name = "multi-stage"
version = "0.2.0"
description = "Multi-stage"

[stages.plan]
prompt = "Plan"

[stages.implement]
prompt = "Implement"

[stages.review]
prompt = "Review"
"#;
        std::fs::write(&manifest_path, content).unwrap();

        let info = read_blueprint_info(&manifest_path, dir.path()).unwrap();
        assert_eq!(info.name, "multi-stage");
        assert_eq!(info.stages.len(), 3);
    }

    #[test]
    fn discover_blueprints_with_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let content = r#"
[agent]
name = "discovered"
version = "1.0.0"
description = "Should be discovered"

[stages.work]
prompt = "Do work"
"#;
        std::fs::write(agent_dir.join("agent.leviath"), content).unwrap();

        let config = crate::config::Config {
            agent_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        let blueprints = discover_blueprints(&config);
        let found = blueprints.iter().find(|b| b.name == "discovered");
        assert_discovered_in_custom_path(found.is_some());
    }

    fn assert_discovered_in_custom_path(found: bool) {
        assert!(found, "should discover agent in custom path");
    }

    #[test]
    #[should_panic(expected = "should discover agent in custom path")]
    fn assert_discovered_in_custom_path_panics_when_not_found() {
        assert_discovered_in_custom_path(false);
    }

    #[test]
    fn discover_blueprints_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            agent_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        // Should not panic even with empty dirs
        let blueprints = discover_blueprints(&config);
        // May include blueprints from ~/.leviath/agents, but no crash
        let _ = blueprints;
    }

    #[test]
    fn discover_blueprints_nonexistent_path_is_skipped() {
        let config = crate::config::Config {
            agent_paths: vec![PathBuf::from("/nonexistent/path/unlikely_to_exist_12345")],
            ..Default::default()
        };
        // Should not panic
        let _ = discover_blueprints(&config);
    }

    #[test]
    fn discover_blueprints_direct_manifest_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"
[agent]
name = "direct"
version = "0.1.0"
description = "Directly in scan dir"

[stages.run]
prompt = "Run"
"#;
        std::fs::write(dir.path().join("agent.leviath"), content).unwrap();

        let config = crate::config::Config {
            agent_paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        let blueprints = discover_blueprints(&config);
        let found = blueprints.iter().find(|b| b.name == "direct");
        assert_discovered_directly_in_scan_dir(found.is_some());
    }

    fn assert_discovered_directly_in_scan_dir(found: bool) {
        assert!(found, "should discover agent.leviath directly in scan dir");
    }

    #[test]
    #[should_panic(expected = "should discover agent.leviath directly in scan dir")]
    fn assert_discovered_directly_in_scan_dir_panics_when_not_found() {
        assert_discovered_directly_in_scan_dir(false);
    }
}
