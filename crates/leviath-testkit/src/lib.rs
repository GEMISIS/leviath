//! Internal test support shared across the workspace's test suites.
//!
//! Consumed only as a dev-dependency. Excluded from the per-package 100%
//! coverage gate for the same reason `xtask` is: every line here executes
//! inside other packages' gated test suites, and self-gating test scaffolding
//! forces tests-of-test-helpers with no defect-finding power.
//!
//! Shared here so each helper has exactly one definition: copy-pasted
//! per-package `test_support.rs` versions drift silently (`AlwaysOnSubscriber`
//! reached five copies in two divergent designs, the raw-TCP mock server nine).

use std::sync::{Mutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A no-op `tracing::Subscriber` that reports every callsite and level as
/// enabled.
///
/// Without a registered subscriber, `tracing`'s macros short-circuit
/// field-expression evaluation before the "is this level enabled" check even
/// runs, so a line like `tracing::debug!(status = %status, "...")` shows a
/// nonzero hit count for the macro call while its field-expansion sub-region
/// shows zero - even though the enclosing branch genuinely ran. Installing
/// this for a test's duration makes those field expressions actually
/// evaluate, which is what the coverage gate needs to see.
pub struct AlwaysOnSubscriber;

impl tracing::Subscriber for AlwaysOnSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let _ = self.enabled(event.metadata());
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(tracing::metadata::LevelFilter::TRACE)
    }
}

/// Install [`AlwaysOnSubscriber`] as the *thread-local* default for the
/// lifetime of the returned guard.
///
/// Unlike [`with_tracing`]'s process-global install, every caller gets its
/// own fully functioning subscriber regardless of test execution order -
/// which matters when a test needs the subscriber active on a specific
/// thread the global path may have missed.
pub fn tracing_guard() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(AlwaysOnSubscriber)
}

/// Run `f` with [`AlwaysOnSubscriber`] installed as the process-global
/// default subscriber.
///
/// `set_global_default` only succeeds once per test binary, so the install
/// is latched behind a `OnceLock`; the interest cache is rebuilt so
/// callsites that were resolved before the install re-evaluate against it.
pub fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(AlwaysOnSubscriber);
        tracing::callsite::rebuild_interest_cache();
    });
    f()
}

/// Serializes tests that swap the process-global panic hook. Under the
/// parallel test runner, two such tests interleave: one test's `set_hook`
/// replaces the other's silencing closure before the other's panic fires, so
/// that closure never runs and shows as uncovered. Hold this across each
/// test's set-hook, panic, restore-hook sequence (synchronous work only).
pub static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with the panic hook silenced, restoring the previous hook after.
/// `f` is expected to swallow its own panic (e.g. via `catch_unwind`).
pub fn with_silenced_panics<T>(f: impl FnOnce() -> T) -> T {
    let _guard = PANIC_HOOK_LOCK.lock().expect("panic-hook lock poisoned");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let out = f();
    std::panic::set_hook(prev);
    out
}

/// Serve exactly one connection with the given raw response bytes and return
/// the server's `http://...` base URL. The building block under the shaped
/// helpers below; call it directly for a response none of them describe.
pub async fn spawn_mock_raw(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let _ = socket.write_all(&response).await;
        let _ = socket.flush().await;
        let _ = socket.shutdown().await;
    });
    format!("http://{}", addr)
}

/// A one-shot HTTP server answering with the given status line and JSON body
/// (correct `Content-Length`, connection closed after).
pub async fn spawn_mock_server(status: u16, reason: &str, body: impl Into<Vec<u8>>) -> String {
    spawn_mock_server_with_headers(status, reason, "Content-Type: application/json\r\n", body).await
}

/// [`spawn_mock_server`] with caller-supplied extra header lines (each ending
/// in `\r\n`) in place of the default JSON content type.
pub async fn spawn_mock_server_with_headers(
    status: u16,
    reason: &str,
    extra_headers: &str,
    body: impl Into<Vec<u8>>,
) -> String {
    let body = body.into();
    let mut response = format!(
        "HTTP/1.1 {} {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason,
        extra_headers,
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    spawn_mock_raw(response).await
}

/// A one-shot HTTP server that declares a `Content-Length` far larger than
/// the bytes it actually sends, then closes - forcing a genuine I/O error
/// when the caller reads the response body, so `.text()`-failure fallbacks
/// are reachable in tests.
pub async fn spawn_mock_server_truncated_body(status: u16, reason: &str) -> String {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: 10000\r\nConnection: close\r\n\r\nshort",
        status, reason
    );
    spawn_mock_raw(response.into_bytes()).await
}
