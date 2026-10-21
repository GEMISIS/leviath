//! What `lev serve` says on stdout and in its ready file.

use super::*;

use std::time::Duration;

use leviath_runtime::control_socket::{ControlClient, control_id};

use crate::commands::serve::{ServeArgs, StopFuture};

/// A writer a test reads back.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        leviath_core::sync::lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(leviath_core::sync::lock(&self.0).clone()).expect("utf-8")
    }

    /// Every line, each parsed as JSON.
    fn events(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is one JSON object"))
            .collect()
    }
}

fn args(json: bool, ready_file: Option<PathBuf>) -> ServeArgs {
    ServeArgs {
        daemon: Default::default(),
        port: 0,
        name: None,
        host: "127.0.0.1".to_string(),
        cors: None,
        token: Some("secret-test-token".to_string()),
        allow_admin: false,
        print_graphql_schema: false,
        workdir_roots: Vec::new(),
        no_remote_yolo: false,
        tls_cert: None,
        tls_key: None,
        no_remote_seed_commands: false,
        max_concurrent_requests: None,
        request_timeout_secs: Some(0),
        exit_on_stdin_eof: false,
        shutdown_grace_secs: 1,
        json,
        ready_file,
        spawn_timeout_secs: None,
    }
}

/// An announcer over `args` writing to a buffer, and how many events its
/// prose watch was told.
fn announcer(args: &ServeArgs) -> (Arc<Announcer>, Buffer, Arc<Mutex<usize>>) {
    let out = Buffer::default();
    let told = Arc::new(Mutex::new(0usize));
    let counter = told.clone();
    let prose: StartupWatch = Arc::new(move |_| *leviath_core::sync::lock(&counter) += 1);
    let announce = Announcer::new(args, Box::new(out.clone()), prose);
    (announce, out, told)
}

fn progress(step: &str, done: u64, total: u64, detail: Option<&str>) -> StartupProgress {
    StartupProgress {
        step: step.to_string(),
        done,
        total,
        detail: detail.map(str::to_string),
    }
}

fn bound() -> SocketAddr {
    "127.0.0.1:53712".parse().expect("an address")
}

#[test]
fn starting_lines_carry_every_field_and_a_counting_step_is_rate_limited() {
    let (announce, out, told) = announcer(&args(true, None));
    let watch = announce.clone().watch();
    // Not begun yet: nothing to say.
    watch(StartupEvent::Progress(&progress("", 0, 0, None)));
    watch(StartupEvent::Progress(&progress(
        "converting runs",
        0,
        982,
        None,
    )));
    // The same step counting on, inside the interval: held back.
    watch(StartupEvent::Progress(&progress(
        "converting runs",
        1,
        982,
        None,
    )));
    // A detail, a new total, and a new step are each shown at once.
    watch(StartupEvent::Progress(&progress(
        "converting runs",
        1,
        982,
        Some("/tmp/backup"),
    )));
    watch(StartupEvent::Progress(&progress(
        "converting runs",
        1,
        990,
        Some("/tmp/backup"),
    )));
    watch(StartupEvent::Progress(&progress(
        "loading providers",
        0,
        0,
        None,
    )));
    // Unchanged: nothing new to say, whatever the interval.
    watch(StartupEvent::Progress(&progress(
        "loading providers",
        0,
        0,
        None,
    )));

    // A step's last item is shown, whatever the interval.
    watch(StartupEvent::Progress(&progress("backing up", 0, 2, None)));
    watch(StartupEvent::Progress(&progress("backing up", 2, 2, None)));
    // Ready is announced by the server, not by the watch.
    watch(StartupEvent::Ready);
    let events = out.events();
    assert_eq!(
        events,
        vec![
            serde_json::json!({"event":"daemon_starting","step":"converting runs","done":0,"total":982,"detail":null}),
            serde_json::json!({"event":"daemon_starting","step":"converting runs","done":1,"total":982,"detail":"/tmp/backup"}),
            serde_json::json!({"event":"daemon_starting","step":"converting runs","done":1,"total":990,"detail":"/tmp/backup"}),
            serde_json::json!({"event":"daemon_starting","step":"loading providers","done":0,"total":0,"detail":null}),
            serde_json::json!({"event":"daemon_starting","step":"backing up","done":0,"total":2,"detail":null}),
            serde_json::json!({"event":"daemon_starting","step":"backing up","done":2,"total":2,"detail":null}),
        ]
    );
    // The person on stderr is told everything.
    assert_eq!(*leviath_core::sync::lock(&told), 10);
}

#[test]
fn a_counting_step_is_shown_again_once_the_interval_passes() {
    let (announce, out, _) = announcer(&args(true, None));
    let fast = Announcer {
        every: Duration::ZERO,
        ..Arc::into_inner(announce).expect("one owner")
    };
    fast.progress(&progress("converting runs", 1, 3, None));
    fast.progress(&progress("converting runs", 2, 3, None));
    assert_eq!(out.events().len(), 2);
}

#[test]
fn the_ready_object_names_the_server_and_its_daemon_but_never_the_token() {
    let (announce, out, _) = announcer(&args(true, None));
    let identity = DaemonIdentity::this_process("daemon-build");
    announce.daemon_ready(Some(&identity));
    announce.listening("http", bound()).expect("announced");
    let events = out.events();
    assert_eq!(events[0], serde_json::json!({"event":"daemon_ready"}));
    let ready = &events[1];
    assert_eq!(ready["event"], "ready");
    assert_eq!(ready["schema"], READY_SCHEMA);
    assert_eq!(ready["url"], "http://127.0.0.1:53712");
    assert_eq!(ready["pid"], std::process::id());
    assert_eq!(ready["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(ready["build"], crate::daemon::setup::CURRENT_BUILD);
    assert_eq!(
        ready["server_instance"],
        crate::commands::serve::events::server_instance()
    );
    assert_eq!(
        ready["daemon"],
        serde_json::json!({"pid": identity.pid, "version": identity.version, "build": "daemon-build"})
    );
    assert!(!out.text().contains("secret-test-token"));
}

#[test]
fn a_daemon_that_did_not_say_who_it_is_is_null() {
    let (announce, out, _) = announcer(&args(true, None));
    announce.daemon_ready(None);
    announce.listening("https", bound()).expect("announced");
    let events = out.events();
    assert_eq!(events[1]["daemon"], serde_json::Value::Null);
    assert_eq!(events[1]["url"], "https://127.0.0.1:53712");
}

#[test]
fn without_the_flags_stdout_is_the_one_line_of_prose() {
    let (announce, out, told) = announcer(&args(false, None));
    let watch = announce.clone().watch();
    watch(StartupEvent::Progress(&progress(
        "converting runs",
        0,
        2,
        None,
    )));
    announce.replacing("leviath daemon is older; restarting it");
    announce.spawned(42);
    announce.daemon_ready(None);
    announce.listening("http", bound()).expect("announced");
    let failed = announce.finish(Err(anyhow::anyhow!("never reported on stdout")));
    assert!(failed.is_err());
    assert_eq!(
        out.text(),
        "Leviath API server listening on http://127.0.0.1:53712\n"
    );
    assert_eq!(*leviath_core::sync::lock(&told), 1);
}

#[test]
fn replacing_and_spawning_the_daemon_are_events() {
    let (announce, out, _) = announcer(&args(true, None));
    announce.replacing("leviath daemon is older; restarting it");
    announce.spawned(4242);
    assert_eq!(
        out.events(),
        vec![
            serde_json::json!({"event":"daemon_replacing","message":"leviath daemon is older; restarting it"}),
            serde_json::json!({"event":"daemon_spawned","pid":4242}),
        ]
    );
}

#[test]
fn a_failure_is_one_error_line_with_the_code_of_its_stage() {
    let (announce, out, _) = announcer(&args(true, None));
    assert!(announce.finish(Ok(())).is_ok());
    assert_eq!(out.text(), "");
    announce.stage(ErrorCode::BindFailed);
    let err = anyhow::anyhow!("os error 48").context("port 3000 is in use");
    assert!(announce.finish(Err(err)).is_err());
    assert_eq!(
        out.events(),
        vec![serde_json::json!({
            "event": "error",
            "code": "bind_failed",
            "message": "port 3000 is in use: os error 48",
        })]
    );
}

#[test]
fn the_ready_file_holds_the_ready_object_and_goes_when_the_server_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ready.json");
    let (announce, out, _) = announcer(&args(false, Some(path.clone())));
    announce.daemon_ready(None);
    announce.listening("http", bound()).expect("announced");
    let written = std::fs::read_to_string(&path).expect("the ready file");
    let ready: serde_json::Value = serde_json::from_str(&written).expect("one JSON object");
    assert_eq!(ready["event"], "ready");
    assert_eq!(ready["url"], "http://127.0.0.1:53712");
    // Prose on stdout still, without --json.
    assert!(out.text().starts_with("Leviath API server listening on"));
    assert_ready_file_private(&path);
    announce.stopped();
    assert!(!path.exists());
    // Stopping twice, or with the file already gone, is fine.
    announce.stopped();
}

#[cfg(unix)]
fn assert_ready_file_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// The Windows twin of the Unix check: there is no mode to read, so the file
/// is a file holding one line.
#[cfg(not(unix))]
fn assert_ready_file_private(path: &std::path::Path) {
    let text = std::fs::read_to_string(path).expect("the ready file");
    assert_eq!(text.lines().count(), 1);
}

#[test]
fn a_bare_ready_file_name_is_written_in_the_working_directory_and_removed() {
    let (announce, _, _) = announcer(&args(false, Some(PathBuf::from("ready-961-test.json"))));
    let path = std::env::current_dir()
        .expect("cwd")
        .join("ready-961-test.json");
    assert_eq!(announce.ready_file.as_deref(), Some(path.as_path()));
    announce.listening("http", bound()).expect("announced");
    assert!(path.is_file());
    announce.stopped();
    assert!(!path.exists());
}

#[test]
fn a_ready_file_replaced_since_is_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ready.json");
    let (announce, _, _) = announcer(&args(false, Some(path.clone())));
    // Never ready: nothing of ours to remove.
    std::fs::write(&path, "another server's").expect("write");
    announce.stopped();
    assert!(path.exists());
    announce.listening("http", bound()).expect("announced");
    // Something else wrote over it.
    std::fs::write(&path, "another server's").expect("write");
    announce.stopped();
    assert!(path.exists());
    // Or put a directory there.
    std::fs::remove_file(&path).expect("remove");
    std::fs::create_dir(&path).expect("mkdir");
    announce.stopped();
    assert!(path.is_dir());
}

#[test]
fn a_panic_before_ready_is_one_error_line() {
    let (announce, out, _) = announcer(&args(true, None));
    announce.panicked("boom");
    assert_eq!(
        out.events(),
        vec![serde_json::json!({"event":"error","code":"internal_error","message":"boom"})]
    );
    // While stdout is held, nothing waits on it.
    let held = announce.out.lock().expect("lock");
    announce.panicked("again");
    drop(held);
    // After ready, and without --json, a panic says nothing on stdout.
    announce.listening("http", bound()).expect("announced");
    announce.panicked("late");
    assert_eq!(out.events().len(), 2);
    let (prose, out, _) = announcer(&args(false, None));
    prose.panicked("boom");
    assert_eq!(out.text(), "");
}

#[test]
fn a_ready_file_that_cannot_be_written_fails_the_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("missing").join("ready.json");
    let (announce, out, _) = announcer(&args(true, Some(path)));
    let err = announce
        .listening("http", bound())
        .expect_err("no directory to write into");
    assert!(format!("{err}").contains("ready file"), "{err}");
    assert!(announce.finish(Err(err)).is_err());
    let events = out.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["code"], "ready_file_failed");
}

#[test]
fn a_silent_announcer_tells_nobody() {
    let silent = Announcer::silent(&args(true, None));
    (silent.prose)(StartupEvent::Ready);
    silent.daemon_ready(None);
}

#[tokio::test]
async fn a_test_observer_is_told_the_bound_address() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (announce, _, _) = announcer(&args(false, None));
    let announce = announce.observed_by(Some(tx));
    announce.listening("http", bound()).expect("announced");
    assert_eq!(rx.await.expect("told"), bound());
    // An observer that gave up does not stop the server.
    let (tx, rx) = tokio::sync::oneshot::channel::<SocketAddr>();
    drop(rx);
    let (announce, _, _) = announcer(&args(false, None));
    announce
        .observed_by(Some(tx))
        .listening("http", bound())
        .expect("announced");
}

// ─── A whole server ─────────────────────────────────────────────────────────

fn no_daemon() -> Daemon {
    Daemon {
        control: ControlClient::new(control_id(std::path::Path::new("/no/such/leviath"))),
        identity: Some(DaemonIdentity::this_process("daemon-build")),
    }
}

/// Run `lev serve` with `args` over `daemon` to its end, stopping it once
/// it has said it is ready, and answer what it wrote on stdout. The stage is
/// the one the composition root leaves it at once it has a daemon, or not.
async fn serve(args: ServeArgs, daemon: anyhow::Result<Daemon>) -> (anyhow::Result<()>, Buffer) {
    let (announce, out, _) = announcer(&args);
    announce.stage(ErrorCode::DaemonUnavailable);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop: StopFuture = Box::pin(async move {
        let _ = stop_rx.await;
    });
    let server = tokio::spawn(crate::commands::serve::execute(
        args,
        announce,
        daemon,
        Arc::new(|_: &[String]| anyhow::bail!("no upgrade in a test")),
        stop,
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !server.is_finished() && !out.text().contains("\"event\":\"ready\"") {
        assert!(Instant::now() < deadline, "the server never got ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = stop_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the server stops")
        .expect("no panic");
    (result, out)
}

#[tokio::test]
async fn json_serve_prints_only_json_lines_and_removes_its_ready_file() {
    crate::config::with_isolated_config_path_async("announce-json-ready", |_dir| async move {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ready.json");
        let (result, out) = serve(args(true, Some(path.clone())), Ok(no_daemon())).await;
        result.expect("a clean stop");
        let events = out.events();
        assert_eq!(events[0]["event"], "daemon_ready");
        let ready = &events[1];
        assert_eq!(events.len(), 2);
        let url = ready["url"].as_str().expect("a url");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(
            !url.ends_with(":0"),
            "the bound port, not the asked one: {url}"
        );
        assert_eq!(ready["daemon"]["build"], "daemon-build");
        assert!(!path.exists(), "removed on a clean stop");
    })
    .await;
}

#[tokio::test]
async fn json_serve_ends_with_the_code_of_what_stopped_it() {
    crate::config::with_isolated_config_path_async("announce-json-errors", |_dir| async move {
        // No daemon.
        let (result, out) = serve(args(true, None), Err(anyhow::anyhow!("no daemon"))).await;
        assert!(result.is_err());
        assert_eq!(last_code(&out), "daemon_unavailable");

        // No token.
        let no_token = ServeArgs {
            token: None,
            ..args(true, None)
        };
        let (result, out) = temp_env::async_with_vars(
            [("LEVIATH_API_TOKEN", None::<&str>)],
            serve(no_token, Ok(no_daemon())),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(last_code(&out), "token_missing");

        // TLS flags that do not go together.
        let lone_cert = ServeArgs {
            tls_cert: Some(PathBuf::from("cert.pem")),
            ..args(true, None)
        };
        let (_, out) = serve(lone_cert, Ok(no_daemon())).await;
        assert_eq!(last_code(&out), "tls_invalid");

        // A taken port.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = taken.local_addr().expect("addr").port();
        let (_, out) = serve(
            ServeArgs {
                port,
                ..args(true, None)
            },
            Ok(no_daemon()),
        )
        .await;
        assert_eq!(last_code(&out), "bind_failed");

        // A configuration that does not parse.
        std::fs::write(crate::config::Config::config_path(), "not valid toml [[[").expect("write");
        let (_, out) = serve(args(true, None), Ok(no_daemon())).await;
        assert_eq!(last_code(&out), "config_invalid");
    })
    .await;
}

#[tokio::test]
async fn a_ready_file_that_cannot_be_written_stops_the_server() {
    crate::config::with_isolated_config_path_async(
        "announce-ready-file-fails",
        |_dir| async move {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("missing").join("ready.json");
            let (result, out) = serve(args(true, Some(path)), Ok(no_daemon())).await;
            assert!(result.is_err());
            assert_eq!(last_code(&out), "ready_file_failed");
        },
    )
    .await;
}

/// The code on the last line, which is the one error line.
fn last_code(out: &Buffer) -> String {
    let events = out.events();
    let errors = events.iter().filter(|e| e["event"] == "error").count();
    assert_eq!(errors, 1, "one error line");
    let last = events.last().expect("a line");
    assert_eq!(last["event"], "error");
    last["code"].as_str().expect("a code").to_string()
}
