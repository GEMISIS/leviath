//! `/api/yolo`: the profiles behind `--yolo=<name>`, over HTTP.
//!
//! The same three questions `lev yolo` answers, for a console: what profiles
//! are there, what does one say, and what would it decide for a call. The
//! write half, `PUT /api/yolo`, replaces the file's text and is mounted only
//! under `--allow-admin`, because a profile is a grant of permissions and
//! writing one is the same category of act as writing `config.toml`.
//!
//! Every handler reads the file as it stands through
//! [`crate::yolo::yolo_path`], a plain function over the environment rather
//! than anything reachable from a request, for the reason `AdminPaths` gives:
//! a file location that is request data is a path-injection finding.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::types::{ApiError, AppState, err};
use crate::commands::yolo::TestArgs;
use crate::yolo::{ProfileSummary, YoloError, YoloFile, yolo_path};

/// `GET /api/yolo`: where the file is, whether it loads, and each profile's
/// shape.
#[derive(Debug, Serialize)]
pub(super) struct YoloListing {
    /// The file the profiles are read from.
    pub path: String,
    /// Whether it exists. `false` with no error means `--yolo=<name>` has
    /// nothing to name yet.
    pub exists: bool,
    /// Why the file does not load, when it does not. The profiles are then
    /// empty: a spawn naming one would be refused with this same message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub profiles: Vec<ProfileSummary>,
}

/// The listing for the file as it stands.
fn listing() -> YoloListing {
    let path = yolo_path();
    match YoloFile::load_from(&path) {
        Ok(file) => YoloListing {
            path: path.display().to_string(),
            exists: file.exists(),
            error: None,
            profiles: file.profiles().map(|p| p.summary()).collect(),
        },
        Err(e) => YoloListing {
            path: path.display().to_string(),
            exists: true,
            error: Some(e.to_string()),
            profiles: Vec::new(),
        },
    }
}

/// The status a profile lookup failure answers with: the name is the
/// caller's (404), the file is the operator's (422).
fn lookup_error(e: YoloError) -> ApiError {
    let code = match e {
        YoloError::UnknownProfile { .. } | YoloError::NoFile { .. } => StatusCode::NOT_FOUND,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    err(code, e.to_string())
}

/// Load the file and resolve `name` in it.
fn profile_named(name: &str) -> Result<std::sync::Arc<crate::yolo::YoloProfile>, ApiError> {
    let path = yolo_path();
    let file = YoloFile::load_from(&path).map_err(lookup_error)?;
    file.resolve(Some(name), &path).map_err(lookup_error)
}

/// `GET /api/yolo`.
pub(super) async fn list_profiles(State(_state): State<AppState>) -> Json<YoloListing> {
    Json(listing())
}

/// `GET /api/yolo/{name}`: one profile in full, as its parsed spec, with what
/// it keeps for a person.
pub(super) async fn get_profile(
    State(_state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let profile = profile_named(&name)?;
    Ok(Json(serde_json::json!({
        "name": profile.name,
        "spec": profile.spec,
        "holds": profile.holds(),
    })))
}

/// `POST /api/yolo/test`: what `profile` would decide for one call.
#[derive(Debug, Deserialize)]
pub(super) struct TestReq {
    /// The profile to test.
    pub profile: String,
    /// The tool the model would call.
    pub tool: String,
    /// For the shell: the command line.
    #[serde(default)]
    pub command: Option<String>,
    /// The call's arguments, for a tool that is not the shell.
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    /// The workdir the run would have. Defaults to the server's.
    #[serde(default)]
    pub workdir: Option<String>,
    /// What the config layers resolve the tool to (`allow`, `ask`, `deny`).
    /// Left off, it is read from the config in force.
    #[serde(default)]
    pub configured: Option<String>,
    /// `builtin`, `subagent`, `script` or `mcp`; guessed from the name when
    /// left off.
    #[serde(default)]
    pub kind: Option<String>,
    /// Decide as if `--allow <tool>` had been passed.
    #[serde(default)]
    pub allowed: bool,
}

/// `POST /api/yolo/test`. The same code path as `lev yolo test`, so the two
/// cannot disagree about a call.
pub(super) async fn test_profile(
    State(state): State<AppState>,
    Json(req): Json<TestReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let profile = profile_named(&req.profile)?;
    let args = TestArgs {
        name: req.profile,
        tool: req.tool,
        command: req.command,
        args: req.arguments.map(|v| v.to_string()),
        workdir: req.workdir.map(std::path::PathBuf::from),
        configured: req.configured,
        kind: req.kind,
        allowed: req.allowed,
        json: true,
    };
    let config = state.config.current();
    let configured = crate::commands::yolo::configured_policy(&args, Some(&config))
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let decision = crate::commands::yolo::decision_json(&profile, &args, configured)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(decision))
}

/// `PUT /api/yolo` (admin-only): replace the file's text.
#[derive(Debug, Deserialize)]
pub(super) struct WriteYoloReq {
    /// The whole file, as TOML.
    pub text: String,
}

/// `PUT /api/yolo`. Parse-checked first, so a save that would not load is
/// refused with the same message a spawn would give, and the file on disk is
/// left as it was.
pub(super) async fn put_profiles(
    State(_state): State<AppState>,
    Json(req): Json<WriteYoloReq>,
) -> Result<Json<YoloListing>, ApiError> {
    YoloFile::from_toml(&req.text).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let path = yolo_path();
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &req.text)
    };
    write().map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write {}: {e}", path.display()),
        )
    })?;
    Ok(Json(listing()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::yolo::EXAMPLE_TOML;

    fn state() -> AppState {
        super::super::testutil::state_with_agent_paths(Vec::new())
    }

    fn test_req(profile: &str, tool: &str, command: Option<&str>) -> TestReq {
        TestReq {
            profile: profile.to_string(),
            tool: tool.to_string(),
            command: command.map(str::to_string),
            arguments: None,
            workdir: Some(std::env::temp_dir().display().to_string()),
            configured: None,
            kind: None,
            allowed: false,
        }
    }

    #[tokio::test]
    async fn the_listing_follows_the_file_through_its_states() {
        crate::config::with_isolated_config_path_async("api-yolo-list", |cfg| async move {
            let none = list_profiles(State(state())).await.0;
            assert!(!none.exists);
            assert!(none.error.is_none());
            assert!(none.profiles.is_empty());
            assert_eq!(none.path, cfg.join("yolo.toml").display().to_string());

            let written = put_profiles(
                State(state()),
                Json(WriteYoloReq {
                    text: EXAMPLE_TOML.to_string(),
                }),
            )
            .await
            .expect("a good file is written")
            .0;
            assert!(written.exists);
            assert_eq!(written.profiles.len(), 2);
            assert_eq!(written.profiles[1].name, "careful");
            assert_eq!(
                std::fs::read_to_string(cfg.join("yolo.toml")).unwrap(),
                EXAMPLE_TOML
            );

            // A broken save is refused and leaves the file alone.
            let refused = put_profiles(
                State(state()),
                Json(WriteYoloReq {
                    text: "[p]\nblock = 1\n".to_string(),
                }),
            )
            .await
            .expect_err("a file that does not load is refused");
            assert_eq!(refused.0, StatusCode::BAD_REQUEST);
            assert!(
                refused.1.0.error.contains("does not load"),
                "{}",
                refused.1.0.error
            );
            assert_eq!(
                std::fs::read_to_string(cfg.join("yolo.toml")).unwrap(),
                EXAMPLE_TOML
            );

            // A file broken by hand is reported, with no profiles.
            std::fs::write(cfg.join("yolo.toml"), "[").unwrap();
            let broken = list_profiles(State(state())).await.0;
            assert!(broken.exists);
            assert!(broken.error.is_some());
            assert!(broken.profiles.is_empty());
            let json = serde_json::to_value(&broken).unwrap();
            assert!(json["error"].is_string());
        })
        .await;
    }

    #[tokio::test]
    async fn a_profile_is_read_by_name_or_answers_404() {
        crate::config::with_isolated_config_path_async("api-yolo-get", |cfg| async move {
            let missing = get_profile(State(state()), Path("careful".to_string()))
                .await
                .expect_err("no file");
            assert_eq!(missing.0, StatusCode::NOT_FOUND);
            std::fs::write(cfg.join("yolo.toml"), EXAMPLE_TOML).unwrap();
            let got = get_profile(State(state()), Path("careful".to_string()))
                .await
                .expect("found")
                .0;
            assert_eq!(got["name"], "careful");
            assert_eq!(got["spec"]["default"], "ask");
            assert!(got["holds"].as_array().unwrap().len() >= 2);
            let unknown = get_profile(State(state()), Path("nope".to_string()))
                .await
                .expect_err("unknown");
            assert_eq!(unknown.0, StatusCode::NOT_FOUND);
            assert!(unknown.1.0.error.contains("build-only, careful"));
            std::fs::write(cfg.join("yolo.toml"), "[").unwrap();
            let broken = get_profile(State(state()), Path("careful".to_string()))
                .await
                .expect_err("broken file");
            assert_eq!(broken.0, StatusCode::UNPROCESSABLE_ENTITY);
        })
        .await;
    }

    #[tokio::test]
    async fn a_call_is_decided_the_way_the_cli_decides_it() {
        crate::config::with_isolated_config_path_async("api-yolo-test", |cfg| async move {
            std::fs::write(cfg.join("yolo.toml"), EXAMPLE_TOML).unwrap();
            let denied = test_profile(
                State(state()),
                Json(test_req("careful", "shell", Some("curl https://x"))),
            )
            .await
            .expect("decided")
            .0;
            assert_eq!(denied["policy"], "deny");
            assert_eq!(denied["reason"], "shell deny rule \"curl\"");
            assert_eq!(denied["configured"], "ask");

            let mut req = test_req("careful", "web_fetch", None);
            req.arguments = Some(serde_json::json!({"url": "https://x"}));
            req.configured = Some("allow".to_string());
            let asked = test_profile(State(state()), Json(req))
                .await
                .expect("decided")
                .0;
            assert_eq!(asked["policy"], "ask");

            let mut req = test_req("careful", "acme__thing", None);
            req.kind = Some("robot".to_string());
            let bad = test_profile(State(state()), Json(req))
                .await
                .expect_err("a bad kind");
            assert_eq!(bad.0, StatusCode::BAD_REQUEST);
            let mut req = test_req("careful", "shell", Some("ls"));
            req.configured = Some("maybe".to_string());
            let bad = test_profile(State(state()), Json(req))
                .await
                .expect_err("a bad policy word");
            assert_eq!(bad.0, StatusCode::BAD_REQUEST);
            let unknown = test_profile(State(state()), Json(test_req("nope", "shell", Some("ls"))))
                .await
                .expect_err("unknown profile");
            assert_eq!(unknown.0, StatusCode::NOT_FOUND);
        })
        .await;
    }
}
