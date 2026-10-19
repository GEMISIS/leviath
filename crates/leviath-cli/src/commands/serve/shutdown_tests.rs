use super::*;

use std::io::Write;

/// A stop that has not been asked for, as a future.
fn never() -> StopFuture {
    Box::pin(std::future::pending())
}

/// Whether `stop` resolves within a short wait.
async fn resolves(stop: StopFuture) -> bool {
    tokio::time::timeout(Duration::from_secs(5), stop)
        .await
        .is_ok()
}

#[tokio::test]
async fn without_stdin_only_the_stop_ends_the_wait() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut waiting = either(
        Box::pin(async move {
            let _ = rx.await;
        }),
        None,
    );
    // Nothing has asked yet, so the wait is still pending.
    let early = tokio::time::timeout(Duration::from_millis(50), &mut waiting).await;
    assert!(early.is_err(), "nothing asked the server to stop yet");
    tx.send(()).expect("the wait is still listening");
    assert!(resolves(waiting).await);
}

/// The parent going away closes the write end of our stdin pipe; whatever it
/// had already written is read and thrown away first.
#[tokio::test]
async fn a_closed_pipe_ends_the_wait() {
    let (reader, mut writer) = std::io::pipe().expect("a pipe");
    let mut waiting = either(never(), Some(closed(Box::new(reader))));
    writer
        .write_all(b"ignored\n")
        .expect("the pipe takes bytes");
    let early = tokio::time::timeout(Duration::from_millis(50), &mut waiting).await;
    assert!(early.is_err(), "the pipe is still open");
    drop(writer);
    assert!(resolves(waiting).await, "a closed pipe stops the server");
}

/// A read that fails.
struct Broken;

impl Read for Broken {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the handle is gone"))
    }
}

#[tokio::test]
async fn a_failed_read_ends_the_wait_too() {
    assert!(resolves(either(never(), Some(closed(Box::new(Broken))))).await);
}

#[test]
fn the_close_frame_says_going_away() {
    match going_away() {
        Message::Close(Some(frame)) => assert_eq!(frame.code, 1001),
        other => panic!("not a close frame: {other:?}"),
    }
}

#[tokio::test]
async fn a_shutdown_starts_out_not_requested() {
    use futures_util::FutureExt;

    let shutdown = Shutdown::default();
    assert!(shutdown.requested().now_or_never().is_none());
    shutdown.begin();
    assert!(shutdown.requested().now_or_never().is_some());
}

/// The server finishing and every socket closing ends the drain well inside
/// the grace period.
#[tokio::test]
async fn drain_returns_once_the_server_and_its_sockets_are_done() {
    let shutdown = Shutdown::default();
    let socket = tokio::spawn(shutdown.track(async {}));
    shutdown.begin();
    let started = std::time::Instant::now();
    drain(Box::pin(async {}), shutdown, Duration::from_secs(30)).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    socket.await.expect("the socket task ran");
}

/// A request that never finishes is given the grace period and no more.
#[tokio::test(start_paused = true)]
async fn drain_gives_up_when_the_grace_period_ends() {
    crate::test_support::with_tracing(|| {});
    let shutdown = Shutdown::default();
    shutdown.begin();
    let started = tokio::time::Instant::now();
    drain(never(), shutdown, Duration::from_secs(3)).await;
    assert_eq!(started.elapsed().as_secs(), 3);
}
