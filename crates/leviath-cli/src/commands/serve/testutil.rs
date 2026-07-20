//! Test-only helper: a fake shared-world daemon the serve handlers talk to.

use std::sync::Arc;

use leviath_runtime::control_socket::{
    ControlClient, ControlRequest, ControlResponse, bind_control_listener, control_id,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

/// A control client pointing at an address with no daemon — used by tests that
/// never exercise agent actions (read/websocket/polling/config paths).
pub(super) fn no_daemon_client() -> ControlClient {
    ControlClient::new(control_id(std::path::Path::new("/no/such/daemon")))
}

/// Spin up a fake daemon that answers exactly one control request via `respond`
/// (each serve handler makes a single control op per HTTP request). Returns a
/// client pointed at it, the `TempDir` keeping its socket alive, and the task.
pub(super) fn fake_daemon(
    respond: impl Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
) -> (ControlClient, tempfile::TempDir, JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let respond = Arc::new(respond);
    let handle = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        // Tests always send exactly one valid request per connection.
        let line = lines.next_line().await.unwrap().unwrap();
        let req = serde_json::from_str::<ControlRequest>(&line).unwrap();
        let mut out = serde_json::to_string(&respond(req)).unwrap();
        out.push('\n');
        let _ = write_half.write_all(out.as_bytes()).await;
    });
    (ControlClient::new(id), dir, handle)
}
