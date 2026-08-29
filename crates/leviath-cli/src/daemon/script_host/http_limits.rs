//! Process-wide limits on a script's HTTP: per-host concurrency, timeout,
//! retries and redirect policy. Split out of `script_host.rs` for size.

use super::*;

/// Concurrent script-tool HTTP requests allowed per host; `0` is unbounded.
///
/// A process-wide mirror of `[limits] script_http_max_per_host`, for the same
/// reason [`ALLOW_LOCAL_REDIRECTS`] is one: [`HTTP_CLIENT`] is process-wide and
/// the request path has no handle on the config.
pub(super) static HTTP_MAX_PER_HOST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(4);

/// Apply `[limits] script_http_max_per_host` for this process.
pub(crate) fn set_script_http_max_per_host(max: usize) {
    HTTP_MAX_PER_HOST.store(max, std::sync::atomic::Ordering::Relaxed);
}

/// In-flight script-tool requests per host, and the condvar waiters park on.
pub(super) static IN_FLIGHT: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
pub(super) static IN_FLIGHT_FREED: std::sync::Condvar = std::sync::Condvar::new();

/// A permit to have one request in flight against `host`, released on drop.
///
/// A condvar rather than a `tokio::sync::Semaphore` because script tools run on
/// `spawn_blocking` threads with no runtime handle to await on.
pub(super) struct HostPermit(Option<String>);

impl HostPermit {
    /// Take a permit for `url`'s host, blocking while that host is at its cap.
    ///
    /// A URL with no host (and an unbounded cap) takes no permit at all, so the
    /// unlimited setting costs nothing.
    pub(super) fn acquire(url: &str) -> Self {
        let max = HTTP_MAX_PER_HOST.load(std::sync::atomic::Ordering::Relaxed);
        let Some(host) = host_of(url).filter(|_| max > 0) else {
            return Self(None);
        };
        let mut counts = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while counts.get(&host).copied().unwrap_or(0) >= max {
            counts = IN_FLIGHT_FREED
                .wait(counts)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *counts.entry(host.clone()).or_insert(0) += 1;
        Self(Some(host))
    }
}

impl Drop for HostPermit {
    fn drop(&mut self) {
        let Some(host) = self.0.take() else {
            return;
        };
        let mut counts = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `or_insert(1)` rather than `if let Some(..)`: acquire always inserted
        // the key and this is its only remover, so the missing case cannot
        // happen - and written as a branch it was a region no test could enter.
        let remaining = counts.entry(host.clone()).or_insert(1);
        *remaining = remaining.saturating_sub(1);
        if *remaining == 0 {
            counts.remove(&host);
        }
        drop(counts);
        IN_FLIGHT_FREED.notify_all();
    }
}

/// The host part of `url`, lowercased. `None` when it does not parse.
pub(super) fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_lowercase)
}

/// Seconds a script tool's HTTP request may take; `0` leaves the client's own
/// deadline in charge.
///
/// Applied per request rather than on [`HTTP_CLIENT`] because that static is
/// built lazily at first use, which may be before or after the config is read.
/// A per-request timeout also wins over the client's, so this is the value that
/// actually governs.
pub(super) static HTTP_TIMEOUT_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(30);

/// Apply `[limits] script_http_timeout_secs` for this process.
pub(crate) fn set_script_http_timeout(secs: u64) {
    HTTP_TIMEOUT_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
}

/// The configured per-request deadline, if one is set.
pub(super) fn script_http_timeout() -> Option<Duration> {
    match HTTP_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// How many extra attempts a script tool's request gets after a transport
/// failure that looks transient.
pub(super) const SCRIPT_HTTP_RETRIES: u32 = 2;

/// Whether *redirect hops* may land on loopback / private / link-local
/// addresses.
///
/// The authoritative check is [`DaemonScriptHost::allow_local_network`], a plain
/// field on the host. This atomic exists only because [`HTTP_CLIENT`] is
/// process-wide and its redirect callback runs inside reqwest with no access to
/// the host that started the request. `[security] allow_local_network` is a
/// machine-wide switch, so one value per process is the right granularity -
/// but keep the field authoritative and this a mirror of it, not the reverse:
/// global mutable state read by the main check would make every test that
/// touches it race with every test that doesn't.
///
/// Defaults to `false`, so a path that forgets to initialize it is the safe one.
pub(super) static ALLOW_LOCAL_REDIRECTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The lock every test that touches [`ALLOW_LOCAL_REDIRECTS`] must hold.
///
/// The atomic is process-wide, so in a test binary - where everything runs in
/// one process, in parallel - a test that writes it races every test that reads
/// it, and the loser sees the other test's value with no hint that is what
/// happened. A refusal test quietly succeeds, or a permitted-hop test is refused
/// and blames the thing it was actually checking.
///
/// It lives next to the atomic rather than inside one module's test block
/// because the writers are not all in one module: `setup_daemon_host_with`
/// mirrors the config into it at daemon start-up, so a test that stands up a
/// host is a writer too - and that was the one with no idea it had to take this.
/// An async mutex rather than a `std` one because the writers await: standing up
/// a daemon host is an `async fn`, so a `std` guard held across it is
/// `clippy::await_holding_lock` - and the lint is right, the guard would be
/// pinned to whatever thread the future resumed on. This one also cannot be
/// poisoned, which matters for a lock every test in three modules takes: a test
/// that panicked holding it has already said why, and failing every later test
/// as well would bury that one real failure.
#[cfg(test)]
pub(crate) static REDIRECT_MIRROR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take the [`REDIRECT_MIRROR`] lock from synchronous code.
///
/// Async callers should `REDIRECT_MIRROR.lock().await` instead; this is for the
/// plain `#[test]`s and for a `spawn_blocking` body, neither of which can.
#[cfg(test)]
pub(crate) fn lock_redirect_mirror() -> tokio::sync::MutexGuard<'static, ()> {
    REDIRECT_MIRROR.blocking_lock()
}

/// Apply `[security] allow_local_network` to redirect following for this process.
pub(crate) fn set_local_network_allowed(allow: bool) {
    ALLOW_LOCAL_REDIRECTS.store(allow, std::sync::atomic::Ordering::Relaxed);
}

/// The current value of the [`ALLOW_LOCAL_REDIRECTS`] switch.
pub(super) fn local_network_allowed() -> bool {
    ALLOW_LOCAL_REDIRECTS.load(std::sync::atomic::Ordering::Relaxed)
}
