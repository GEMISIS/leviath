//! Waiting for the operating system to ask a long-lived process to stop.
//!
//! Shared by `lev daemon` and `lev serve`, so both stop on the same requests.
//! The wait is real signal I/O with nothing to unit-test it against, which is
//! why it lives in the composition root.

/// Resolves when the process is asked to stop: Ctrl-C, or SIGTERM, which is
/// how launchd, systemd and `kill` stop a process.
#[cfg(unix)]
pub(super) async fn stop_requested() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Resolves when the process is asked to stop: Ctrl-C or Ctrl-Break in its
/// console, the console window closing, or the system shutting down.
///
/// A listener that cannot be installed never fires, and the others still do.
/// A process started without a console (`CREATE_NO_WINDOW`) receives none of
/// these, which is why `lev serve` also offers `--exit-on-stdin-eof`.
#[cfg(windows)]
pub(super) async fn stop_requested() {
    use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
    let mut ctrl_break = ctrl_break().ok();
    let mut ctrl_close = ctrl_close().ok();
    let mut ctrl_shutdown = ctrl_shutdown().ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        Some(()) = async {
            match ctrl_break.as_mut() {
                Some(listener) => listener.recv().await,
                None => std::future::pending().await,
            }
        } => {}
        Some(()) = async {
            match ctrl_close.as_mut() {
                Some(listener) => listener.recv().await,
                None => std::future::pending().await,
            }
        } => {}
        Some(()) = async {
            match ctrl_shutdown.as_mut() {
                Some(listener) => listener.recv().await,
                None => std::future::pending().await,
            }
        } => {}
    }
}

/// Resolves on Ctrl-C, the one stop request every platform tokio supports
/// can deliver.
#[cfg(not(any(unix, windows)))]
pub(super) async fn stop_requested() {
    let _ = tokio::signal::ctrl_c().await;
}
