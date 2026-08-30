//! Test-only helper: a fake shared-world daemon the serve handlers talk to.

use std::sync::Arc;

use leviath_runtime::control_socket::{
    ControlClient, ControlRequest, ControlResponse, bind_control_listener, control_id,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// A config source that never re-reads anything, for a test with no file to
/// watch. Production watches [`crate::config::Config::config_path`] - see
/// [`AppState::current_config`](super::types::AppState::current_config).
pub(super) fn fixed_config(
    config: crate::config::Config,
) -> Arc<crate::daemon::config_reload::ConfigReloader> {
    Arc::new(crate::daemon::config_reload::ConfigReloader::fixed(config))
}

/// Run `f` with `LEVIATH_HOME` redirected at a fresh scratch root, handing it
/// that root.
///
/// Every path the tools and scripts routes touch - the installed agents
/// directory and the global `.rhai` directory - hangs off that variable, so a
/// test that did not set it would read (and, for the write routes, *write*) a
/// developer's real `~/.leviath`. One `temp_env` call rather than a nested one:
/// it serializes process-wide and holds its lock across the future.
pub(super) async fn with_home<R, Fut>(f: impl FnOnce(std::path::PathBuf) -> Fut) -> R
where
    Fut: std::future::Future<Output = R>,
{
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path().to_path_buf();
    temp_env::async_with_vars(
        [("LEVIATH_HOME", Some(root.clone().into_os_string()))],
        f(root),
    )
    .await
}

/// An `AppState` whose config lists `paths` under `agent_paths` and that
/// talks to no daemon, for routes that only need the blueprint catalog.
pub(super) fn state_with_agent_paths(paths: Vec<std::path::PathBuf>) -> super::types::AppState {
    let (event_tx, _) = tokio::sync::broadcast::channel(64);
    super::types::AppState {
        update_check: Default::default(),
        update_jobs: Default::default(),
        config: fixed_config(crate::config::Config {
            agent_paths: paths,
            ..Default::default()
        }),
        event_tx,
        control: no_daemon_client(),
        mcp: super::mcp::McpAdmin::default(),
        limits: Default::default(),
    }
}

/// An `AppState` watching the real file at `path`, so a test can edit it and
/// see the server notice. The counterpart to [`state_with_agent_paths`], whose
/// config is fixed and watches nothing.
pub(super) fn state_with_config_at(path: &std::path::Path) -> super::types::AppState {
    let (event_tx, _) = tokio::sync::broadcast::channel(64);
    super::types::AppState {
        update_check: Default::default(),
        update_jobs: Default::default(),
        config: Arc::new(crate::daemon::config_reload::ConfigReloader::new(
            path.to_path_buf(),
            crate::config::Config::load_from_path_public(path).expect("a config that loads"),
        )),
        event_tx,
        control: no_daemon_client(),
        mcp: super::mcp::McpAdmin::default(),
        limits: Default::default(),
    }
}

/// A control client pointing at an address with no daemon - used by tests that
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
        let stream = listener
            .accept()
            .await
            .expect("accept succeeds")
            .expect("our own connection is admitted");
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

/// Minimal hand-rolled WebSocket client used to drive `handle_ws` end to
/// end over a real TCP loopback connection. No `tokio-tungstenite` or
/// other WS crate is added as a dependency - this speaks just enough of
/// RFC 6455 to perform the opening handshake and exchange text/close
/// frames with axum's server-side WebSocket implementation.
pub(super) struct WsTestClient {
    stream: TcpStream,
}

/// `stream.read()` returning `0` mid-handshake means the peer closed the
/// connection before sending a complete response.
fn assert_handshake_byte_read(n: usize) {
    assert_ne!(n, 0, "connection closed before handshake completed");
}

#[test]
#[should_panic(expected = "connection closed before handshake completed")]
fn assert_handshake_byte_read_panics_on_zero() {
    assert_handshake_byte_read(0);
}

fn assert_handshake_101(response: &str) {
    #[rustfmt::skip]
    assert!(response.starts_with("HTTP/1.1 101"), "expected 101 Switching Protocols, got: {response}");
}

#[test]
#[should_panic(expected = "expected 101 Switching Protocols, got: HTTP/1.1 404 Not Found")]
fn assert_handshake_101_panics_on_non_101() {
    assert_handshake_101("HTTP/1.1 404 Not Found");
}

impl WsTestClient {
    pub(super) async fn connect(addr: std::net::SocketAddr, path: &str) -> Self {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Connection: Upgrade\r\n\
             Upgrade: websocket\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        // Read until the end of the HTTP response headers (\r\n\r\n).
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await.unwrap();
            assert_handshake_byte_read(n);
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let response = String::from_utf8_lossy(&buf);
        assert_handshake_101(&response);

        Self { stream }
    }

    /// Wrap a stream that has already completed its own handshake, for a test
    /// that drove the upgrade itself.
    pub(super) fn from_stream(stream: TcpStream) -> Self {
        Self { stream }
    }

    /// Send a masked text frame (client → server frames must be masked
    /// per RFC 6455).
    pub(super) async fn send_text(&mut self, text: &str) {
        self.send_frame(0x1, text.as_bytes()).await;
    }

    /// Send a masked close frame.
    pub(super) async fn send_close(&mut self) {
        self.send_frame(0x8, &[]).await;
    }

    pub(super) async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        let mut frame = vec![0x80 | opcode];
        let mask: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else {
            frame.push(0x80 | 126);
            frame.push((len >> 8) as u8);
            frame.push(len as u8);
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        self.stream.write_all(&frame).await.unwrap();
    }

    /// Read one server frame (unmasked) and return (opcode, payload).
    ///
    /// Each read names the frame part it was after, so a server that hung up
    /// mid-stream fails with "the server closed the connection before ..."
    /// rather than a bare `Os { code: 54 }` from an anonymous `unwrap`.
    pub(super) async fn recv_frame(&mut self) -> (u8, Vec<u8>) {
        let mut header = [0u8; 2];
        self.stream
            .read_exact(&mut header)
            .await
            .expect("the server closed the connection before the frame header");
        let opcode = header[0] & 0x0f;
        let mut len = (header[1] & 0x7f) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            self.stream
                .read_exact(&mut ext)
                .await
                .expect("the server closed the connection before the 16-bit length");
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.stream
                .read_exact(&mut ext)
                .await
                .expect("the server closed the connection before the 64-bit length");
            len = u64::from_be_bytes(ext) as usize;
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.stream
                .read_exact(&mut payload)
                .await
                .expect("the server closed the connection before the frame payload");
        }
        (opcode, payload)
    }

    /// Drop the TCP connection without a close frame, the way a browser tab
    /// that goes away does. The server sees a reset, not a handshake.
    pub(super) fn close_abruptly(self) {
        drop(self.stream);
    }

    /// Read a single byte, returning `None` on a clean EOF (used to
    /// detect that the server has closed the connection).
    pub(super) async fn recv_eof(&mut self) -> Option<u8> {
        let mut byte = [0u8; 1];
        match self.stream.read(&mut byte).await {
            Ok(0) => None,
            Ok(_) => Some(byte[0]),
            Err(_) => None,
        }
    }
}
