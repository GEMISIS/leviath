//! How `lev serve` stops.
//!
//! A stop is asked for by a future: the composition root's signal wait, and,
//! with `--exit-on-stdin-eof`, the parent closing our stdin. Once it resolves
//! the server stops accepting, every WebSocket is closed with 1001 (going
//! away), and requests already in flight get up to `--shutdown-grace-secs` to
//! answer. Then [`super::execute`] returns `Ok`, so the process exits 0. The
//! daemon is never told anything: it serves other clients and outlives this
//! one.

use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, close_code};
use futures_util::future::{Either, select};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// A future that resolves once, when something asks the server to stop.
pub type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Where a server's long-lived connections learn that it is stopping, and how
/// it waits for them to say goodbye.
///
/// On the shared state, so every WebSocket handler holds the same one. A
/// default value is a server that has not been asked to stop.
#[derive(Clone, Default)]
pub(crate) struct Shutdown {
    requested: CancellationToken,
    sockets: TaskTracker,
}

impl Shutdown {
    /// Tell every connection the server is stopping.
    pub(super) fn begin(&self) {
        self.requested.cancel();
    }

    /// Resolves once [`Shutdown::begin`] has been called.
    pub(super) async fn requested(&self) {
        self.requested.cancelled().await
    }

    /// Whether [`Shutdown::begin`] has been called.
    pub(super) fn is_requested(&self) -> bool {
        self.requested.is_cancelled()
    }

    /// `socket` as a connection the server waits for before it exits, so a
    /// close frame it is sending is not cut off by the process ending.
    pub(super) fn track<F: Future>(
        &self,
        socket: F,
    ) -> tokio_util::task::task_tracker::TrackedFuture<F> {
        self.sockets.track_future(socket)
    }

    /// Resolves once every tracked connection has finished.
    async fn sockets_closed(&self) {
        self.sockets.close();
        self.sockets.wait().await
    }
}

/// The close frame a WebSocket gets when the server stops: 1001, going away.
pub(super) fn going_away() -> Message {
    Message::Close(Some(CloseFrame {
        code: close_code::AWAY,
        reason: "server shutting down".into(),
    }))
}

/// `stop`, or `stdin` reaching its end, whichever comes first.
///
/// Without `stdin` nothing is read, so a server started from a terminal or a
/// service manager keeps its stdin to itself and stops only on `stop`.
pub(super) fn either(stop: StopFuture, stdin: Option<StopFuture>) -> StopFuture {
    let closed = stdin.unwrap_or(Box::pin(std::future::pending()));
    Box::pin(async move {
        let _ = select(stop, closed).await;
    })
}

/// Resolves when `input` reaches its end or fails to read.
///
/// Read on a thread of its own rather than tokio's blocking pool: a blocking
/// read cannot be cancelled, and the runtime waits for its blocking pool on
/// the way out, so a pending read there would keep a server that was stopped
/// by a signal from ever exiting. A plain thread dies with the process.
/// Whatever arrives is discarded; only the end means anything.
pub(super) fn closed(mut input: Box<dyn Read + Send>) -> StopFuture {
    let (gone, ended) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let _gone = gone;
        let mut buf = [0u8; 4096];
        while input.read(&mut buf).is_ok_and(|n| n > 0) {}
    });
    Box::pin(async move {
        let _ = ended.await;
        tracing::info!("stdin closed, so the server is stopping");
    })
}

/// Run `serving` until it finishes and every WebSocket has closed, or until
/// `grace` has passed since the stop began, whichever is first.
///
/// `serving` is the server itself, already wired to call
/// [`Shutdown::begin`] when it is asked to stop. A request still running when
/// the grace period ends is abandoned, and the log says so.
pub(super) async fn drain(serving: StopFuture, shutdown: Shutdown, grace: Duration) {
    let finished = Box::pin(async {
        serving.await;
        shutdown.sockets_closed().await;
    });
    let deadline = Box::pin(async {
        shutdown.requested().await;
        tokio::time::sleep(grace).await;
    });
    if let Either::Right(_) = select(finished, deadline).await {
        let grace_secs = grace.as_secs();
        tracing::warn!(
            grace_secs,
            "requests were still in flight when the grace period ended; stopping anyway"
        );
    }
}

#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod tests;
