//! `/api/fs/dirs` - the directory surface behind the console's folder picker.
//!
//! `GET` is read-only metadata about the host filesystem: one directory level
//! per request, subdirectory names only, so the browser can let the user
//! *choose* an agent workdir without typing a path blind. The spawn endpoint
//! already accepts arbitrary workdirs subject to `--workdir-root`; this route
//! shows exactly the directories that endpoint would accept, under the same
//! fence.
//!
//! `POST` makes one. A browser cannot open a native OS dialog onto the serving
//! machine, so the "New Folder" button every file dialog has had for thirty
//! years has nowhere to come from - without it, "start this agent in a fresh
//! directory" means leaving the console for a terminal, `mkdir`, and coming
//! back. It adds no reach the API does not already grant: it creates one empty
//! directory in a place the caller has already proved it can list and could
//! already have pointed a run at.

use std::path::{Component, Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;

/// `GET /api/fs/dirs?path=<abs>`: list one directory's immediate
/// subdirectories, so the browser's folder picker can walk the tree.
///
/// Without `path`, lists the serve process's working directory - or
/// `--workdir-root` itself when a root is set and the cwd falls outside it,
/// so the picker never *opens* somewhere it cannot stay. A given `path` must
/// be absolute, and with a root set must resolve inside it under the same
/// symlink-aware containment the spawn endpoint applies to workdirs
/// ([`leviath_core::resolves_within`]). Dotted names are excluded, unreadable
/// children are silently skipped, and `parent` is `null` both at the
/// filesystem root and at the workdir-root - the UI is never led above the
/// fence. Purely a filesystem read: it works with the daemon down.
pub(super) async fn list_dirs(
    State(state): State<AppState>,
    Query(query): Query<DirsQuery>,
) -> Result<Json<DirsResp>, (StatusCode, Json<ErrorResponse>)> {
    let root = state.limits.workdir_root.as_deref();
    let cwd = known_dir_or_fs_root(std::env::current_dir().ok());

    let listed = match &query.path {
        Some(p) => {
            let requested = PathBuf::from(p);
            if !requested.is_absolute() {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "path must be absolute".to_string(),
                ));
            }
            if let Some(root) = root
                && !leviath_core::resolves_within(&requested, root)
            {
                return Err(err(
                    StatusCode::FORBIDDEN,
                    format!("path '{p}' is outside the configured --workdir-root"),
                ));
            }
            requested
        }
        // No path: the picker's opening view - the cwd, clamped to the root
        // when one is set and the cwd falls outside it.
        None => match root {
            Some(root) if !leviath_core::resolves_within(&cwd, root) => root.to_path_buf(),
            _ => cwd.clone(),
        },
    };

    match std::fs::metadata(&listed) {
        Ok(m) if !m.is_dir() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("'{}' is a file, not a directory", listed.display()),
            ));
        }
        Ok(_) => {}
        Err(_) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                format!("directory '{}' not found", listed.display()),
            ));
        }
    }

    let entries = std::fs::read_dir(&listed).map_err(|e| {
        err(
            StatusCode::NOT_FOUND,
            format!("could not read '{}': {e}", listed.display()),
        )
    })?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !query.hidden && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // `metadata` follows symlinks, so a link to a directory counts as one;
        // a child that cannot be stat'd (dangling link, no permission) is
        // silently skipped - the picker shows what it can offer, not errors.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        // With a root set, a symlink child can still point outside the fence;
        // offering it would be offering a workdir the spawn endpoint refuses.
        if let Some(root) = root
            && !leviath_core::resolves_within(&path, root)
        {
            continue;
        }
        dirs.push(DirEntry {
            name,
            path: path.to_string_lossy().into_owned(),
        });
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));

    // `null` at the filesystem root, and at the workdir-root: "up one level"
    // from the fence would land somewhere every other request here refuses.
    let parent = match root.is_some_and(|r| same_dir(&listed, r)) {
        true => None,
        false => listed.parent().map(|p| p.to_string_lossy().into_owned()),
    };

    Ok(Json(DirsResp {
        path: listed.to_string_lossy().into_owned(),
        parent,
        home: known_dir_or_fs_root(dirs::home_dir())
            .to_string_lossy()
            .into_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        root: root.map(|r| r.to_string_lossy().into_owned()),
        dirs,
    }))
}

/// `POST /api/fs/dirs`: create one empty directory inside a directory the
/// caller can already list.
///
/// The body names the parent and a single new segment rather than one joined
/// path, so the containment check runs on ground the caller has already proved
/// it can reach and a `name` carrying separators is malformed input rather
/// than something the fence has to catch. Every guard mirrors
/// [`list_dirs`]: absolute parent, inside `--workdir-root` when one is set,
/// the parent must exist and be a directory. One level, `create_dir` rather
/// than `create_dir_all`, matching the listing route's one-directory-level
/// shape - and an existing target is a 409 rather than a silent success, so a
/// picker can tell "I made it" from "it was already there".
pub(super) async fn create_dir(
    State(state): State<AppState>,
    Json(req): Json<MkdirReq>,
) -> Result<(StatusCode, Json<MkdirResp>), (StatusCode, Json<ErrorResponse>)> {
    let parent = PathBuf::from(&req.path);
    if !parent.is_absolute() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "path must be absolute".to_string(),
        ));
    }
    if let Some(root) = state.limits.workdir_root.as_deref()
        && !leviath_core::resolves_within(&parent, root)
    {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!(
                "path '{}' is outside the configured --workdir-root",
                req.path
            ),
        ));
    }
    match std::fs::metadata(&parent) {
        Ok(m) if !m.is_dir() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("'{}' is a file, not a directory", parent.display()),
            ));
        }
        Ok(_) => {}
        Err(_) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                format!("directory '{}' not found", parent.display()),
            ));
        }
    }
    if !is_one_segment(&req.name) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("name '{}' must be a single directory name", req.name),
        ));
    }

    let target = parent.join(&req.name);
    // `create_dir` already fails on an existing path, but its error does not
    // say *which* failure it was, and "already there" is the one a picker has
    // to render differently from "the machine said no".
    //
    // `symlink_metadata`, not `exists`: a dangling symlink is something at that
    // name too. `exists` follows the link, finds nothing, and would send this
    // to `create_dir` for an `EEXIST` reported as a 500.
    if target.symlink_metadata().is_ok() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("'{}' already exists", target.display()),
        ));
    }
    std::fs::create_dir(&target).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not create '{}': {e}", target.display()),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(MkdirResp {
            path: target.to_string_lossy().into_owned(),
            parent: parent.to_string_lossy().into_owned(),
        }),
    ))
}

/// Whether `name` is one ordinary directory name rather than a path.
///
/// Deliberately not [`leviath_core::is_safe_path_component`], whose
/// `[A-Za-z0-9._-]` allowlist is right for a name that becomes a *Leviath*
/// identifier (a blueprint name, a run id) and wrong for one the user is
/// simply typing into a folder picker: `My Project` and `notas-españolas` are
/// ordinary directory names, and refusing them would be a surprise rather than
/// a safety property.
///
/// What matters here is only that `parent.join(name)` cannot leave `parent`,
/// which a single `Normal` component cannot: separators, `.`, `..`, a leading
/// `/`, and a Windows drive prefix all parse as something other than one
/// `Normal`. A backslash is rejected explicitly because Unix parses it as an
/// ordinary character, and a directory named `a\b` is a path on the next
/// machine that reads it.
fn is_one_segment(name: &str) -> bool {
    !name.contains('\\')
        && matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(only)] if *only == std::ffi::OsStr::new(name)
        )
}

/// The filesystem root when a well-known directory (cwd, home) cannot be
/// determined - the picker always has *somewhere* real to stand.
fn known_dir_or_fs_root(dir: Option<PathBuf>) -> PathBuf {
    dir.unwrap_or_else(|| PathBuf::from("/"))
}

/// Whether two paths name the same directory, symlinks resolved - so the
/// root-fence check holds when the listed path spells the root differently
/// (e.g. through a symlinked ancestor like macOS's `/tmp`).
fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::commands::serve::testutil::no_daemon_client;
    use crate::config::Config;

    fn app_with_root(workdir_root: Option<PathBuf>) -> Router {
        let (tx, _) = broadcast::channel(64);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Arc::new(ServeLimits {
                workdir_root,
                ..Default::default()
            }),
        };
        Router::new()
            .route("/api/fs/dirs", get(list_dirs).post(create_dir))
            .with_state(state)
    }

    /// POST `/api/fs/dirs` with `{path, name}`, returning status and body.
    async fn post_dir(app: Router, path: &str, name: &str) -> (StatusCode, Vec<u8>) {
        let body = serde_json::json!({ "path": path, "name": name }).to_string();
        let req = Request::builder()
            .method("POST")
            .uri("/api/fs/dirs")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    fn made_of(body: &[u8]) -> MkdirResp {
        serde_json::from_slice(body).unwrap()
    }

    /// GET `/api/fs/dirs[?path=<path>]`, returning status and body. (`path`
    /// goes into the query string verbatim - every path these tests use is
    /// query-safe as-is.)
    async fn get_dirs(app: Router, path: Option<&str>) -> (StatusCode, Vec<u8>) {
        let uri = match path {
            Some(p) => format!("/api/fs/dirs?path={p}"),
            None => "/api/fs/dirs".to_string(),
        };
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    fn listing_of(body: &[u8]) -> DirsResp {
        serde_json::from_slice(body).unwrap()
    }

    fn error_of(body: &[u8]) -> String {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn list_dirs_defaults_to_the_serve_process_cwd() {
        let (status, body) = get_dirs(app_with_root(None), None).await;
        assert_eq!(status, StatusCode::OK);
        let got = listing_of(&body);
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(got.path, cwd.to_string_lossy());
        assert_eq!(got.cwd, cwd.to_string_lossy());
        assert!(got.root.is_none());
        assert_eq!(
            got.home,
            dirs::home_dir().unwrap().to_string_lossy().into_owned()
        );
        // The tests run from the crate directory, whose `src/` must be listed.
        assert!(got.dirs.iter().any(|d| d.name == "src"), "{:?}", got.dirs);
    }

    #[tokio::test]
    async fn list_dirs_lists_an_explicit_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (status, body) =
            get_dirs(app_with_root(None), Some(&dir.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let got = listing_of(&body);
        assert_eq!(got.path, dir.path().to_string_lossy());
        // No root: `parent` is the plain filesystem parent.
        assert_eq!(
            got.parent.as_deref(),
            Some(dir.path().parent().unwrap().to_string_lossy().as_ref())
        );
        assert_eq!(got.dirs.len(), 1);
        assert_eq!(got.dirs[0].name, "sub");
        assert_eq!(got.dirs[0].path, dir.path().join("sub").to_string_lossy());
    }

    #[tokio::test]
    async fn list_dirs_returns_subdirectories_only_name_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zeta")).unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("mid")).unwrap();
        std::fs::write(dir.path().join("a-file.txt"), "not a dir").unwrap();
        let (status, body) =
            get_dirs(app_with_root(None), Some(&dir.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }

    #[tokio::test]
    async fn list_dirs_excludes_dotted_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("visible")).unwrap();
        let (status, body) =
            get_dirs(app_with_root(None), Some(&dir.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["visible"]);
    }

    #[tokio::test]
    async fn list_dirs_hidden_true_includes_dotted_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("visible")).unwrap();
        let uri = format!(
            "/api/fs/dirs?path={}&hidden=true",
            dir.path().to_string_lossy()
        );
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app_with_root(None).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, [".git", "visible"]);
    }

    #[tokio::test]
    async fn list_dirs_relative_path_returns_400() {
        let (status, body) = get_dirs(app_with_root(None), Some("some/relative")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_of(&body), "path must be absolute");
    }

    #[tokio::test]
    async fn list_dirs_missing_directory_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let (status, body) = get_dirs(app_with_root(None), Some(&missing.to_string_lossy())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            error_of(&body),
            format!("directory '{}' not found", missing.display())
        );
    }

    #[tokio::test]
    async fn list_dirs_file_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, "not a directory").unwrap();
        let (status, body) = get_dirs(app_with_root(None), Some(&file.to_string_lossy())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            error_of(&body),
            format!("'{}' is a file, not a directory", file.display())
        );
    }

    #[tokio::test]
    async fn list_dirs_refuses_a_path_outside_the_workdir_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, body) = get_dirs(app, Some(&outside.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            error_of(&body),
            format!(
                "path '{}' is outside the configured --workdir-root",
                outside.path().display()
            )
        );
    }

    /// At the workdir-root itself `parent` is `null` - the picker is never
    /// offered a step above the fence - while a subdirectory's parent is real.
    #[tokio::test]
    async fn list_dirs_parent_is_null_at_the_workdir_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();

        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, body) = get_dirs(app, Some(&root.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let got = listing_of(&body);
        assert!(got.parent.is_none());
        assert_eq!(got.root.as_deref(), Some(root.path().to_str().unwrap()));

        let app = app_with_root(Some(root.path().to_path_buf()));
        let inside = root.path().join("inside");
        let (status, body) = get_dirs(app, Some(&inside.to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let got = listing_of(&body);
        assert_eq!(
            got.parent.as_deref(),
            Some(root.path().to_string_lossy().as_ref())
        );
    }

    /// With a root set and the serve process's cwd outside it, the default
    /// listing opens at the root, not somewhere the picker cannot stay.
    #[tokio::test]
    async fn list_dirs_default_is_clamped_to_the_workdir_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("only")).unwrap();
        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, body) = get_dirs(app, None).await;
        assert_eq!(status, StatusCode::OK);
        let got = listing_of(&body);
        assert_eq!(got.path, root.path().to_string_lossy());
        assert!(got.parent.is_none());
        assert_eq!(got.dirs.len(), 1);
        assert_eq!(got.dirs[0].name, "only");
        // `cwd` still reports where the process actually runs.
        assert_eq!(
            got.cwd,
            std::env::current_dir().unwrap().to_string_lossy().as_ref()
        );
    }

    /// A symlink to a directory is a directory to the picker - unless a root
    /// is set and the link resolves outside it, in which case offering it
    /// would be offering a workdir the spawn endpoint refuses.
    #[cfg(unix)]
    #[tokio::test]
    async fn list_dirs_symlinks_out_of_the_root_are_excluded() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();

        // With the root set, the escaping link is not offered.
        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, body) = get_dirs(app, Some(&root.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["inside"]);

        // Without one, a symlink-to-dir is listed like any directory.
        let (status, body) =
            get_dirs(app_with_root(None), Some(&root.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["escape", "inside"]);
    }

    /// A directory that exists but cannot be read (no read permission) is
    /// reported, not a 500.
    #[cfg(unix)]
    #[tokio::test]
    async fn list_dirs_unreadable_directory_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let (status, body) = get_dirs(app_with_root(None), Some(&locked.to_string_lossy())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let msg = error_of(&body);
        let expected = format!("could not read '{}'", locked.display());
        assert!(msg.starts_with(&expected), "{msg}");

        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
    }

    // ─── POST /api/fs/dirs ────────────────────────────────────────────────

    /// The whole point of the route: a directory that was not there is, and
    /// the listing the picker re-runs afterwards shows it.
    #[tokio::test]
    async fn create_dir_makes_the_directory_and_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().into_owned();
        let (status, body) = post_dir(app_with_root(None), &parent, "new-thing").await;
        assert_eq!(status, StatusCode::CREATED);
        let made = made_of(&body);
        assert_eq!(made.parent, parent);
        assert_eq!(made.path, dir.path().join("new-thing").to_string_lossy());
        assert!(dir.path().join("new-thing").is_dir(), "it is really there");

        let (status, body) = get_dirs(app_with_root(None), Some(&parent)).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["new-thing"]);
    }

    /// A folder picker's New Folder has to accept the names people give
    /// folders. A dotted one is allowed too: `hidden=true` already lists them,
    /// so refusing to make one would be an inconsistency, not a guard.
    #[tokio::test]
    async fn create_dir_accepts_ordinary_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().into_owned();
        for name in ["My Project", "notas-españolas", ".hidden", "v1.2.3"] {
            let (status, _) = post_dir(app_with_root(None), &parent, name).await;
            assert_eq!(status, StatusCode::CREATED, "{name}");
            assert!(dir.path().join(name).is_dir(), "{name}");
        }
    }

    /// A `name` that is a path, not a name, is malformed input - caught here
    /// rather than left for the containment fence, which is why the body takes
    /// the parent and the segment separately.
    #[tokio::test]
    async fn create_dir_refuses_a_name_that_is_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().into_owned();
        for name in ["a/b", "..", ".", "", "/abs", "../escape", "a\\b"] {
            let (status, body) = post_dir(app_with_root(None), &parent, name).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
            assert_eq!(
                error_of(&body),
                format!("name '{name}' must be a single directory name")
            );
        }
        // Nothing was created on the way past the guard.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn create_dir_relative_parent_returns_400() {
        let (status, body) = post_dir(app_with_root(None), "some/relative", "x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_of(&body), "path must be absolute");
    }

    #[tokio::test]
    async fn create_dir_missing_parent_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let (status, body) = post_dir(app_with_root(None), &missing.to_string_lossy(), "x").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            error_of(&body),
            format!("directory '{}' not found", missing.display())
        );
    }

    #[tokio::test]
    async fn create_dir_parent_that_is_a_file_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, "not a directory").unwrap();
        let (status, body) = post_dir(app_with_root(None), &file.to_string_lossy(), "x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            error_of(&body),
            format!("'{}' is a file, not a directory", file.display())
        );
    }

    /// The same fence as the listing: a parent the `GET` refuses to show is a
    /// parent the `POST` refuses to write into.
    #[tokio::test]
    async fn create_dir_refuses_a_parent_outside_the_workdir_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, body) = post_dir(app, &outside.path().to_string_lossy(), "x").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            error_of(&body),
            format!(
                "path '{}' is outside the configured --workdir-root",
                outside.path().display()
            )
        );
        assert!(!outside.path().join("x").exists());

        // The control: inside the fence the same request succeeds, so the 403
        // is the fence and not something else about the request.
        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, _) = post_dir(app, &root.path().to_string_lossy(), "x").await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// "It was already there" is a different answer from "I made it", and a
    /// picker renders them differently.
    #[tokio::test]
    async fn create_dir_on_an_existing_name_returns_409() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().into_owned();
        std::fs::create_dir(dir.path().join("taken")).unwrap();
        let (status, body) = post_dir(app_with_root(None), &parent, "taken").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            error_of(&body),
            format!("'{}' already exists", dir.path().join("taken").display())
        );

        // A file of that name is taken too - `create_dir` would fail anyway,
        // and 409 says why better than the OS error would.
        std::fs::write(dir.path().join("afile"), "x").unwrap();
        let (status, _) = post_dir(app_with_root(None), &parent, "afile").await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    /// The machine refusing is neither a bad request nor a missing one: a name
    /// past the filesystem's per-component limit comes back as a 500 carrying
    /// what the OS said.
    #[tokio::test]
    async fn create_dir_reports_what_the_filesystem_refused() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().into_owned();
        let too_long = "n".repeat(300);
        let (status, body) = post_dir(app_with_root(None), &parent, &too_long).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let msg = error_of(&body);
        let expected = format!("could not create '{}", dir.path().display());
        assert!(msg.starts_with(&expected), "{msg}");
    }

    /// One level, like the listing route: the parent has to exist already.
    #[tokio::test]
    async fn create_dir_does_not_make_intermediate_levels() {
        let dir = tempfile::tempdir().unwrap();
        let two_deep = dir.path().join("a");
        let (status, _) = post_dir(app_with_root(None), &two_deep.to_string_lossy(), "b").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!dir.path().join("a").exists(), "nothing was created");
    }

    /// The cwd/home fallback: a known directory passes through untouched, an
    /// unknowable one becomes the filesystem root.
    #[test]
    fn known_dir_or_fs_root_falls_back_to_the_fs_root() {
        assert_eq!(
            known_dir_or_fs_root(Some(PathBuf::from("/somewhere"))),
            PathBuf::from("/somewhere")
        );
        assert_eq!(known_dir_or_fs_root(None), PathBuf::from("/"));
    }

    /// `same_dir` falls back to literal comparison when a path cannot be
    /// canonicalized (it does not exist).
    #[test]
    fn same_dir_compares_noncanonicalizable_paths_literally() {
        assert!(same_dir(
            Path::new("/no/such/dir/anywhere"),
            Path::new("/no/such/dir/anywhere")
        ));
        assert!(!same_dir(
            Path::new("/no/such/dir/anywhere"),
            Path::new("/no/such/dir/elsewhere")
        ));
    }

    /// A child that cannot be stat'd (here: a dangling symlink) is skipped,
    /// never an error - the picker shows what it can offer.
    #[cfg(unix)]
    #[tokio::test]
    async fn list_dirs_skips_unreadable_children() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fine")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling")).unwrap();
        let (status, body) =
            get_dirs(app_with_root(None), Some(&dir.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["fine"]);
    }

    /// Windows twin of `list_dirs_symlinks_out_of_the_root_are_excluded`.
    /// Windows spells a directory link `symlink_dir`; the fence behaves the
    /// same, because `resolves_within` canonicalizes before comparing.
    #[cfg(windows)]
    #[tokio::test]
    async fn list_dirs_symlinks_out_of_the_root_are_excluded_windows() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();
        std::os::windows::fs::symlink_dir(outside.path(), root.path().join("escape")).unwrap();

        let app = app_with_root(Some(root.path().to_path_buf()));
        let (status, body) = get_dirs(app, Some(&root.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["inside"]);

        let (status, body) =
            get_dirs(app_with_root(None), Some(&root.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["escape", "inside"]);
    }

    /// Windows twin of `list_dirs_unreadable_directory_is_reported`.
    ///
    /// There is no `0o000` for a directory on Windows, but a sharing
    /// violation refuses a read the same way: an exclusive (no-share) handle
    /// on the directory leaves `metadata` working - it asks for no access,
    /// and falls back to `FindFirstFileEx` on a sharing violation anyway - so
    /// the request gets past the is-a-directory check and fails where the
    /// Unix one does, in `read_dir`.
    #[cfg(windows)]
    #[tokio::test]
    async fn list_dirs_unreadable_directory_is_reported_windows() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        // `GENERIC_READ` - opening for anything less than a read would not
        // conflict with the listing `read_dir` is about to attempt.
        const GENERIC_READ: u32 = 0x8000_0000;
        // `FILE_FLAG_BACKUP_SEMANTICS`, without which a directory cannot be
        // opened as a handle at all.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let handle = OpenOptions::new()
            .access_mode(GENERIC_READ)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&locked)
            .unwrap();

        let (status, body) = get_dirs(app_with_root(None), Some(&locked.to_string_lossy())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let msg = error_of(&body);
        let expected = format!("could not read '{}'", locked.display());
        assert!(msg.starts_with(&expected), "{msg}");

        // Released before the tempdir's own cleanup runs against it.
        drop(handle);
    }

    /// Windows twin of `list_dirs_skips_unreadable_children`: a directory
    /// link whose target does not exist. `metadata` follows the link, finds
    /// nothing, and reports a plain not-found - the one Windows failure it
    /// does not paper over with a `FindFirstFileEx` fallback - so the child
    /// is skipped rather than listed or raised.
    #[cfg(windows)]
    #[tokio::test]
    async fn list_dirs_skips_unreadable_children_windows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fine")).unwrap();
        std::os::windows::fs::symlink_dir(dir.path().join("gone"), dir.path().join("dangling"))
            .unwrap();
        let (status, body) =
            get_dirs(app_with_root(None), Some(&dir.path().to_string_lossy())).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<String> = listing_of(&body).dirs.into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["fine"]);
    }
}
