//! `GET /api/fs/dirs` - directory browsing for the console's folder picker.
//!
//! Read-only metadata about the host filesystem: one directory level per
//! request, subdirectory names only, so the browser can let the user *choose*
//! an agent workdir without typing a path blind. The spawn endpoint already
//! accepts arbitrary workdirs subject to `--workdir-root`; this route shows
//! exactly the directories that endpoint would accept, under the same fence.

use std::path::{Path, PathBuf};

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
            config: Arc::new(Config::default()),
            event_tx: tx,
            control: no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Arc::new(ServeLimits {
                workdir_root,
                ..Default::default()
            }),
        };
        Router::new()
            .route("/api/fs/dirs", get(list_dirs))
            .with_state(state)
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
}
