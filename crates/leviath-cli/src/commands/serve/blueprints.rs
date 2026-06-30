//! Blueprint discovery, CRUD, and validation endpoints.

use std::path::{Path, PathBuf};

use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use crate::commands::run::parse_manifest_public;

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
            if let Some(info) = read_blueprint_info(&manifest, &dir) {
                results.push(info);
            }
        }
        // Check subdirs
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let m = p.join("agent.leviath");
                    if m.exists() {
                        if let Some(info) = read_blueprint_info(&m, &p) {
                            results.push(info);
                        }
                    }
                }
            }
        }
    }

    results
}

pub(super) fn read_blueprint_info(manifest_path: &Path, dir: &Path) -> Option<BlueprintInfo> {
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let bp = parse_manifest_public(&content).ok()?;
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
    // Validate manifest first
    if let Err(e) = parse_manifest_public(&body.manifest) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        ));
    }

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

    read_blueprint_info(&manifest_path, &dir)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to read back created blueprint".to_string(),
                }),
            )
        })
}

pub(super) async fn update_blueprint(
    AxumPath(name): AxumPath<String>,
    Json(body): Json<UpdateBlueprintReq>,
) -> Result<Json<BlueprintInfo>, (StatusCode, Json<ErrorResponse>)> {
    if let Err(e) = parse_manifest_public(&body.manifest) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid manifest: {}", e),
            }),
        ));
    }

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

    read_blueprint_info(&manifest_path, &dir)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to read back updated blueprint".to_string(),
                }),
            )
        })
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
    match parse_manifest_public(&body.manifest) {
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
        assert!(
            blueprints
                .iter()
                .any(|b| b["name"].as_str() == Some("test-bp")),
            "test-bp should be listed"
        );
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

    // ─── create_blueprint ─────────────────────────────────────────────────────

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

    // ─── update_blueprint ─────────────────────────────────────────────────────

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
        assert!(found.is_some(), "should discover agent in custom path");
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
        assert!(
            found.is_some(),
            "should discover agent.leviath directly in scan dir"
        );
    }
}
