//! The per-run routes that read a run's record, its logs and its working
//! directory, against runs laid down the way those readers find them: a
//! record, a stage index and log files, and files in a workdir.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use tokio::sync::broadcast;
use tower::ServiceExt;

use super::*;
use crate::commands::serve::testutil::fake_daemon;
use crate::config::Config;
use crate::runstate::{RunMeta, RunStatus, create_run};

/// A control client at an address with no daemon (read endpoints don't use it).
fn no_daemon() -> ControlClient {
    ControlClient::new(leviath_runtime::control_socket::control_id(
        std::path::Path::new("/no/such/daemon"),
    ))
}

fn test_state() -> AppState {
    let (tx, _) = broadcast::channel(64);
    AppState {
        caches: Default::default(),
        signer: Default::default(),
        update_check: Default::default(),
        update_jobs: Default::default(),
        config: crate::commands::serve::testutil::fixed_config(Config::default()),
        event_tx: tx,
        control: no_daemon(),
        mcp: crate::commands::serve::mcp::McpAdmin::default(),
        providers: crate::commands::serve::providers::ProviderAdmin::default(),
        limits: Default::default(),
    }
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
        status: leviath_core::run_meta::StageRunStatus::Complete,
        entered: true,
        ..leviath_core::run_meta::StageRecord::new(name.to_string(), index)
    }
}

/// Call `GET /api/runs/{id}/logs{query}` and return the body as text.
/// The endpoint's whole bug was that its body was always empty, so a log
/// test that does not read the body proves nothing.
async fn logs_body(run_id: &str, query: &str) -> String {
    let app = Router::new()
        .route("/api/runs/{id}/logs", get(run_logs))
        .with_state(test_state());
    let req = Request::builder()
        .uri(format!("/api/runs/{run_id}/logs{query}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ─── run_logs ───────────────────────────────────────────────────────────

#[tokio::test]
async fn run_logs_reads_the_current_stage_not_the_dead_run_level_file() {
    crate::runstate::with_isolated_runs_dir_async(
        "agent_logs_reads_current_stage",
        |_d| async move {
            let run_id = unique_run_id("logs-ok");
            let meta = make_run(&run_id);
            create_run(&meta).unwrap();

            // Two stages, so "current" has to mean the last one and not
            // just "the only one that exists".
            runstate::write_stages_index(&run_id, &[stage_rec(0, "plan"), stage_rec(1, "code")])
                .unwrap();
            runstate::append_stage_output(&run_id, 0, "from the planning stage");
            runstate::append_stage_output(&run_id, 1, "from the coding stage");
            // A run-level `output.log` is not a log source. Nothing
            // writes it in production; planting it here proves the
            // handler reads only the stage files.
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
async fn run_logs_selects_a_stage_a_stream_and_every_stage() {
    crate::runstate::with_isolated_runs_dir_async(
        "agent_logs_selects_stage_and_stream",
        |_d| async move {
            let run_id = unique_run_id("logs-select");
            let meta = make_run(&run_id);
            create_run(&meta).unwrap();
            runstate::write_stages_index(&run_id, &[stage_rec(0, "plan"), stage_rec(1, "code")])
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
async fn run_logs_tail_bounds_the_bytes_returned() {
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
async fn run_logs_is_empty_when_no_stages_exist() {
    crate::runstate::with_isolated_runs_dir_async("agent_logs_no_stages", |_d| async move {
        let run_id = unique_run_id("logs-no-stages");
        let meta = make_run(&run_id);
        create_run(&meta).unwrap();
        // No stages.json at all. A stray run-level file is not a log
        // source either: only stage files are.
        std::fs::write(
            runstate::run_dir(&run_id).join("output.log"),
            "not a log source",
        )
        .unwrap();

        assert_eq!(logs_body(&run_id, "").await, "");

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    })
    .await;
}

#[tokio::test]
async fn run_logs_rejects_an_unparseable_stage_or_stream() {
    crate::runstate::with_isolated_runs_dir_async(
        "agent_logs_rejects_bad_params",
        |_d| async move {
            let run_id = unique_run_id("logs-bad");
            let meta = make_run(&run_id);
            create_run(&meta).unwrap();

            for query in ["?stage=middle", "?stream=everything"] {
                let app = Router::new()
                    .route("/api/runs/{id}/logs", get(run_logs))
                    .with_state(test_state());
                let req = Request::builder()
                    .uri(format!("/api/runs/{run_id}/logs{query}"))
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
async fn run_logs_nonexistent_run_returns_404() {
    let app = Router::new()
        .route("/api/runs/{id}/logs", get(run_logs))
        .with_state(test_state());
    let req = Request::builder()
        .uri("/api/runs/nonexistent-run-xyz-logs/logs")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

// ─── run_file ───────────────────────────────────────────────────────────

fn files_app() -> Router {
    Router::new()
        .route("/api/runs/{id}/files", get(run_file))
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
    let uri = format!("/api/runs/{id}/files?path={path}&offset={offset}");
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = files_app().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body.to_vec())
}

/// GET `/api/runs/{id}/files?path=<path>`, returning status and body.
/// (`path` goes into the query string verbatim - every path these tests
/// use is query-safe as-is.)
async fn get_file(id: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let uri = format!("/api/runs/{id}/files?path={path}");
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

/// `GET /api/runs/{id}/files` with no `path`, plus any extra query.
async fn list_files(id: &str, extra: &str) -> (StatusCode, serde_json::Value) {
    let uri = format!("/api/runs/{id}/files{extra}");
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

/// Every listing entry is typed by the registry from its name: a file gets
/// its mime type, a directory gets none.
#[tokio::test]
async fn a_listing_types_each_entry_by_name() {
    crate::runstate::with_isolated_runs_dir_async("agent_files_mime", |_d| async move {
        let workdir = tempfile::tempdir().unwrap();
        std::fs::write(workdir.path().join("hero.png"), "x").unwrap();
        std::fs::create_dir(workdir.path().join("assets")).unwrap();
        let run_id = unique_run_id("files-mime");
        create_run_in(&run_id, workdir.path());

        let (status, listing) = list_files(&run_id, "?source=workdir").await;
        assert_eq!(status, StatusCode::OK);
        let by_name: std::collections::HashMap<&str, &serde_json::Value> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["name"].as_str().unwrap(), e))
            .collect();
        // Typed by extension, not sniffed: the bytes above are not a PNG.
        assert_eq!(by_name["hero.png"]["mime_type"], "image/png");
        // A directory carries no type.
        assert_eq!(by_name["assets"]["mime_type"], "");

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
        for i in 0..crate::commands::serve::core::files::MAX_LISTING_ENTRIES + 5 {
            std::fs::write(workdir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let run_id = unique_run_id("files-many");
        create_run_in(&run_id, workdir.path());

        let (status, listing) = list_files(&run_id, "?source=workdir").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            listing["entries"].as_array().unwrap().len(),
            crate::commands::serve::core::files::MAX_LISTING_ENTRIES
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
        std::os::windows::fs::symlink_dir(outside.path(), workdir.path().join("escape")).unwrap();
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
async fn run_file_unknown_run_returns_404() {
    let (status, body) = get_file("nonexistent-run-xyz-files", "report.md").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_of(&body), "Run 'nonexistent-run-xyz-files' not found");
}

#[tokio::test]
async fn run_file_reads_a_relative_path_within_the_workdir() {
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
async fn run_file_reads_an_absolute_path_within_the_workdir() {
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
async fn run_file_refuses_a_path_outside_the_workdir() {
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
async fn run_file_is_not_fooled_by_a_symlink() {
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
async fn run_file_missing_file_returns_404() {
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
async fn run_file_deleted_workdir_is_a_client_error() {
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
/// Asking for a directory is the natural way to say "what is in here", so
/// the route lists it rather than refusing.
async fn run_file_directory_lists_instead_of_erroring() {
    crate::runstate::with_isolated_runs_dir_async("agent_file_directory_lists", |_d| async move {
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
    })
    .await;
}

/// A file the server cannot open (here: no read permission) is reported,
/// not a 500.
#[cfg(unix)]
#[tokio::test]
async fn run_file_unreadable_file_is_reported() {
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
async fn run_file_unreadable_file_is_reported_windows() {
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
async fn run_file_caps_the_read_at_one_mib() {
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
async fn run_file_truncation_mid_character_stays_text() {
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
async fn run_file_binary_returns_415() {
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

// ─── run_result ─────────────────────────────────────────────────────────

/// The endpoint served a 64 KiB log tail long before an agent could say
/// "here is my answer". Both are returned now: the tail says what the run
/// did, `final_output` says what it concluded.
#[tokio::test]
async fn run_result_serves_the_submitted_answer_beside_the_log_tail() {
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
            runstate::write_stages_index(&run_id, &[stage_rec(0, "summary")]).unwrap();
            runstate::append_stage_output(&run_id, 0, "ran some tools\n");

            let app = Router::new()
                .route("/api/runs/{id}/result", get(run_result))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/runs/{}/result", run_id))
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
async fn run_result_reports_no_answer_as_null() {
    crate::runstate::with_isolated_runs_dir_async(
        "agent_result_reports_no_answer_as_null",
        |_d| async move {
            let run_id = unique_run_id("result-none");
            let meta = make_run(&run_id);
            create_run(&meta).unwrap();
            let app = Router::new()
                .route("/api/runs/{id}/result", get(run_result))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/runs/{}/result", run_id))
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

#[tokio::test]
async fn run_result_existing_run_no_stages() {
    crate::runstate::with_isolated_runs_dir_async(
        "agent_result_existing_run_no_stages",
        |_d| async move {
            let run_id = unique_run_id("result-no-stages");
            let mut meta = make_run(&run_id);
            meta.status = RunStatus::Complete;
            create_run(&meta).unwrap();

            let app = Router::new()
                .route("/api/runs/{id}/result", get(run_result))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/runs/{}/result", run_id))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(result["run_id"].as_str().unwrap(), run_id);
            // A run with no stage recorded has no log tail.
            assert_eq!(result["output"].as_str().unwrap(), "");
            // The word every other route uses, not `Display`'s `Complete`.
            assert_eq!(result["status"].as_str().unwrap(), "complete");

            let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
        },
    )
    .await;
}

#[tokio::test]
async fn run_result_existing_run_with_stages() {
    crate::runstate::with_isolated_runs_dir_async(
        "agent_result_existing_run_with_stages",
        |_d| async move {
            let run_id = unique_run_id("result-stages");
            let meta = make_run(&run_id);
            create_run(&meta).unwrap();

            let stages = vec![runstate::StageRecord::new("plan".to_string(), 0)];
            runstate::write_stages_index(&run_id, &stages).unwrap();
            runstate::append_stage_output(&run_id, 0, "stage output here");

            let app = Router::new()
                .route("/api/runs/{id}/result", get(run_result))
                .with_state(test_state());
            let req = Request::builder()
                .uri(format!("/api/runs/{}/result", run_id))
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
async fn run_result_nonexistent_run_returns_404() {
    let app = Router::new()
        .route("/api/runs/{id}/result", get(run_result))
        .with_state(test_state());
    let req = Request::builder()
        .uri("/api/runs/nonexistent-run-xyz-result/result")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

// ─── cancel_run ───────────────────────────────────────────────────────────

async fn cancel(control: ControlClient, id: &str) -> StatusCode {
    let (tx, _) = broadcast::channel(16);
    let state = AppState {
        caches: Default::default(),
        signer: Default::default(),
        update_check: Default::default(),
        update_jobs: Default::default(),
        config: crate::commands::serve::testutil::fixed_config(Config::default()),
        event_tx: tx,
        control,
        mcp: crate::commands::serve::mcp::McpAdmin::default(),
        providers: crate::commands::serve::providers::ProviderAdmin::default(),
        limits: Default::default(),
    };
    let app = Router::new()
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/runs/{id}/cancel"))
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn cancel_run_cancels_via_daemon() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
    assert_eq!(cancel(control, "run-a").await, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn cancel_run_unknown_run_is_404() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
    assert_eq!(cancel(control, "ghost").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_run_unexpected_response_is_500() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    assert_eq!(
        cancel(control, "a").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn cancel_run_daemon_absent_is_503() {
    assert_eq!(
        cancel(no_daemon(), "a").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

// ─── pause_run / resume_run ──────────────────────────────────────────

/// POST `/api/runs/{id}/pause` (or `/resume`) against a router holding
/// `control`, returning the response status.
async fn post_run_action(control: ControlClient, id: &str, action: &str) -> StatusCode {
    use axum::routing::post;
    let (tx, _) = broadcast::channel(16);
    let state = AppState {
        caches: Default::default(),
        signer: Default::default(),
        update_check: Default::default(),
        update_jobs: Default::default(),
        config: crate::commands::serve::testutil::fixed_config(Config::default()),
        event_tx: tx,
        control,
        mcp: crate::commands::serve::mcp::McpAdmin::default(),
        providers: crate::commands::serve::providers::ProviderAdmin::default(),
        limits: Default::default(),
    };
    let app = Router::new()
        .route("/api/runs/{id}/pause", post(pause_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/runs/{id}/{action}"))
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn pause_run_sends_pause_to_the_daemon() {
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
        post_run_action(control, "run-a", "pause").await,
        StatusCode::NO_CONTENT
    );
}

/// A run that has already finished answers 409, not 404: "not found" about
/// a run sitting in the listing is what sends somebody hunting for a wrong
/// run id. The daemon is never asked, so the answer holds with it down.
#[tokio::test]
async fn pausing_a_finished_run_is_a_conflict() {
    crate::runstate::with_isolated_runs_dir_async("rest-pause-finished", |_d| async move {
        let mut meta = RunMeta::new(
            "run-done".to_string(),
            "test-agent".to_string(),
            "/agents/test".to_string(),
            "do the thing".to_string(),
            None,
            "/work".to_string(),
            1,
        );
        meta.status = crate::runstate::RunStatus::Complete;
        crate::runstate::create_run(&meta).expect("run written");
        assert_eq!(
            post_run_action(no_daemon(), "run-done", "pause").await,
            StatusCode::CONFLICT
        );
    })
    .await;
}

#[tokio::test]
async fn pause_run_refused_is_404() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
    assert_eq!(
        post_run_action(control, "ghost", "pause").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn pause_run_unexpected_response_is_500() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    assert_eq!(
        post_run_action(control, "a", "pause").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn pause_run_daemon_absent_is_503() {
    assert_eq!(
        post_run_action(no_daemon(), "a", "pause").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn resume_run_sends_resume_to_the_daemon() {
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
        post_run_action(control, "run-a", "resume").await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn resume_run_refused_is_404() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
    assert_eq!(
        post_run_action(control, "ghost", "resume").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn resume_run_unexpected_response_is_500() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    assert_eq!(
        post_run_action(control, "a", "resume").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn resume_run_daemon_absent_is_503() {
    assert_eq!(
        post_run_action(no_daemon(), "a", "resume").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

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
