//! How many requests `lev serve` takes on at once, and how long it gives each.
//!
//! Without these, a client that opened a thousand connections would hold a
//! thousand handlers, and a route that never answered would hold its
//! connection for as long as the client cared to wait.
//!
//! Two ceilings, both configurable in the config file and on the command line
//! and both switched off with `0`:
//!
//! - a cap on requests in flight, over which the next request is answered
//!   `503` at once rather than queued, so a flood is refused instead of
//!   piling up behind whatever is slow;
//! - a timeout per request, after which the handler is dropped and the client
//!   gets `408`. Dropping it does not undo what it had already started: a
//!   read on the blocking pool runs to its end.
//!
//! A spawn has a deadline of its own, `spawn_timeout_secs`, longer than the
//! ordinary one because starting a run connects its MCP servers and runs its
//! seeds first. The spawn routes apply it themselves (see [`OWN_DEADLINE`]),
//! since only they know what they were asked: a spawn cut off at its deadline
//! says its outcome is unknown and names the idempotency key to send it again
//! under, because a spawn the daemon took is a run whether or not anyone hears
//! back.
//!
//! The websocket routes are outside both. A subscription is meant to stay open
//! for hours.
//!
//! Both ceilings end when the handler answers, not when the body has been
//! sent. Most routes build their whole body before answering, but the byte
//! routes (`files/raw`, `blobs`, `artifacts` and `exports`) send a file's body
//! a block at a time afterwards. A large download is not cut at the deadline,
//! and does not hold a slot while it streams.
//!
//! A few routes are outside the deadline only, listed in [`DEADLINE_EXEMPT`]
//! with the reason each: they wait on a browser, an MCP server's handshake or
//! a spawned run, all of which take longer than the default 30 s by design.
//! They still take a permit, so the cap holds over them.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::types::err;

/// The two ceilings as numbers, the way the flags and `[serve]` state them.
///
/// `0` means "no limit" on either. This is what `GET /api/config` reports, so
/// a client sees the value in force rather than the default it would guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RequestLimits {
    /// Requests in flight at once before the next is refused with 503.
    pub(super) max_concurrent_requests: u64,
    /// Seconds a request may take before it is answered 408.
    pub(super) request_timeout_secs: u64,
    /// Seconds a spawn may take before it is answered 408, in place of
    /// `request_timeout_secs`.
    #[serde(default = "default_spawn")]
    pub(super) spawn_timeout_secs: u64,
    /// Bytes one request body may carry, which is what bounds a multipart
    /// upload. `[serve] max_upload_bytes`; no flag.
    #[serde(default = "default_upload")]
    pub(super) max_upload_bytes: u64,
}

fn default_upload() -> u64 {
    crate::config::DEFAULT_MAX_UPLOAD_BYTES
}

fn default_spawn() -> u64 {
    crate::config::DEFAULT_SPAWN_TIMEOUT_SECS
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_concurrent_requests: crate::config::DEFAULT_MAX_CONCURRENT_REQUESTS,
            request_timeout_secs: crate::config::DEFAULT_REQUEST_TIMEOUT_SECS,
            spawn_timeout_secs: crate::config::DEFAULT_SPAWN_TIMEOUT_SECS,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        }
    }
}

impl RequestLimits {
    /// Reconcile a flag with its config key: a flag that was typed wins, a
    /// config value stands otherwise, and the config's own default fills in
    /// when neither was set.
    pub(super) fn resolve(flags: Flags, config: &crate::config::ServeConfig) -> Self {
        Self {
            max_concurrent_requests: flags
                .max_concurrent_requests
                .unwrap_or(config.max_concurrent_requests),
            request_timeout_secs: flags
                .request_timeout_secs
                .unwrap_or(config.request_timeout_secs),
            spawn_timeout_secs: flags
                .spawn_timeout_secs
                .unwrap_or(config.spawn_timeout_secs),
            max_upload_bytes: config.max_upload_bytes,
        }
    }

    /// The per-request deadline, or `None` when it is switched off.
    pub(super) fn timeout(&self) -> Option<Duration> {
        seconds(self.request_timeout_secs)
    }

    /// The deadline for a spawn, or `None` when it is switched off.
    pub(super) fn spawn_timeout(&self) -> Option<Duration> {
        seconds(self.spawn_timeout_secs)
    }

    /// The in-flight cap, or `None` when it is switched off.
    fn cap(&self) -> Option<usize> {
        // A cap larger than the platform's `usize` is no cap at all.
        (self.max_concurrent_requests > 0)
            .then(|| usize::try_from(self.max_concurrent_requests).unwrap_or(usize::MAX))
    }
}

/// The `lev serve` flags that set a limit, each `None` when it was not
/// typed.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Flags {
    /// `--max-concurrent-requests`.
    pub(super) max_concurrent_requests: Option<u64>,
    /// `--request-timeout-secs`.
    pub(super) request_timeout_secs: Option<u64>,
    /// `--spawn-timeout-secs`.
    pub(super) spawn_timeout_secs: Option<u64>,
}

/// `secs` as a deadline, where `0` is none.
fn seconds(secs: u64) -> Option<Duration> {
    (secs > 0).then_some(Duration::from_secs(secs))
}

/// `work`, cut off at `deadline` when there is one: `None` when it ran past.
pub(super) async fn within<T>(
    deadline: Option<Duration>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    match deadline {
        Some(deadline) => tokio::time::timeout(deadline, work).await.ok(),
        None => Some(work.await),
    }
}

/// The limits plus the semaphore that enforces the cap, shared by every
/// request through the layer.
pub(super) struct Gate {
    limits: RequestLimits,
    /// `None` when the cap is off, so a disabled cap costs nothing per request
    /// rather than acquiring from a semaphore that can never run out.
    in_flight: Option<Arc<Semaphore>>,
}

impl Gate {
    pub(super) fn new(limits: RequestLimits) -> Arc<Self> {
        // Tokio's semaphore holds at most `MAX_PERMITS` permits; a cap above
        // that is clamped rather than panicking, which for a value nobody will
        // ever reach in flight is the same thing.
        let in_flight = limits
            .cap()
            .map(|cap| Arc::new(Semaphore::new(cap.min(Semaphore::MAX_PERMITS))));
        Arc::new(Self { limits, in_flight })
    }
}

/// Whether a path is one both limits leave alone: the websocket routes, which
/// hold their connection open by design.
fn is_exempt(path: &str) -> bool {
    path == "/ws" || path.starts_with("/ws/")
}

/// The routes that take a permit but get no deadline, as path patterns where
/// `*` stands for one path segment.
///
/// Each of these waits on something slower than a request has any right to
/// be, and cutting it at the deadline does worse than a slow answer: the
/// handler future is dropped with whatever it was holding, and the client
/// gets a 408 for work that was going fine. They still count against the cap,
/// so a flood of them is refused like any other.
pub(super) const DEADLINE_EXEMPT: &[&str] = &[
    // The OAuth flow: opens the operator's browser and waits up to 300 s
    // (`leviath_mcp::auth::CALLBACK_TIMEOUT`) for the consent page to redirect
    // to the loopback listener. Dropping it mid-wait loses the listener and
    // the PKCE state, so the redirect lands on a closed port.
    "/api/mcp/servers/*/login",
    // Connects to the server and lists its tools, under the MCP client's own
    // deadlines: 30 s for the handshake and 120 s for the listing, both
    // longer than the default request deadline.
    "/api/mcp/servers/*/test",
    // The billed doctor: two provider calls under the probe's 60 s request
    // timeout, then a throwaway run through the daemon waited on for up to
    // 90 s. It is admin-only and single-flight already.
    "/api/doctor/live",
];

/// The routes that take a permit and keep their own deadline, by method and
/// path: the spawn routes, which apply `spawn_timeout_secs` and say what a
/// spawn cut off by it means, and `/graphql`, whose deadline depends on the
/// operation it carries (a `spawnRun` or `validateSpawn` mutation gets the
/// spawn's) and so is applied once the operation is parsed.
pub(super) const OWN_DEADLINE: &[(&str, &str)] = &[
    ("POST", "/api/runs"),
    ("POST", "/api/runs/validate"),
    ("POST", "/graphql"),
];

/// Whether the layer leaves `req` without a deadline: one of the
/// [`DEADLINE_EXEMPT`] routes, or one of the [`OWN_DEADLINE`] ones.
fn deadline_exempt(req: &Request) -> bool {
    let path = req.uri().path();
    DEADLINE_EXEMPT
        .iter()
        .any(|pattern| matches_route(pattern, path))
        || OWN_DEADLINE
            .iter()
            .any(|(method, route)| req.method().as_str() == *method && path == *route)
}

/// Segment-by-segment match of `path` against `pattern`, where `*` stands
/// for exactly one non-empty segment. Nothing matches a prefix or a suffix.
fn matches_route(pattern: &str, path: &str) -> bool {
    let mut want = pattern.split('/');
    let mut have = path.split('/');
    loop {
        match (want.next(), have.next()) {
            (None, None) => return true,
            (Some("*"), Some(segment)) if !segment.is_empty() => {}
            (Some(expected), Some(segment)) if expected == segment => {}
            _ => return false,
        }
    }
}

/// The layer itself. Outermost after CORS, so a request the auth layer
/// refuses still counted against the cap while it was being refused: a cap
/// that only covered authenticated requests would be one an unauthenticated
/// flood could walk straight past.
pub(super) async fn limit_requests(
    State(gate): State<Arc<Gate>>,
    req: Request,
    next: Next,
) -> Response {
    if is_exempt(req.uri().path()) {
        return next.run(req).await;
    }

    // Held until the response is built, then released. A byte route's body
    // is sent after that, so a long download does not hold a slot.
    let _permit = match &gate.in_flight {
        None => None,
        Some(semaphore) => match Arc::clone(semaphore).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "too many requests in flight (the cap is {})",
                        gate.limits.max_concurrent_requests
                    ),
                )
                .into_response();
            }
        },
    };

    let deadline = match gate.limits.timeout() {
        Some(deadline) if !deadline_exempt(&req) => deadline,
        _ => return next.run(req).await,
    };
    match tokio::time::timeout(deadline, next.run(req)).await {
        Ok(response) => response,
        Err(_) => err(
            StatusCode::REQUEST_TIMEOUT,
            format!(
                "request took longer than {} s",
                gate.limits.request_timeout_secs
            ),
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::routing::{get, post};
    use tokio::sync::Notify;
    use tower::ServiceExt;

    use crate::config::ServeConfig;

    fn app(limits: RequestLimits, router: Router) -> Router {
        router.layer(axum::middleware::from_fn_with_state(
            Gate::new(limits),
            limit_requests,
        ))
    }

    fn request(path: &str) -> Request {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    fn post_request(path: &str) -> Request {
        Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    async fn error_of(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        parsed["error"].as_str().unwrap().to_string()
    }

    /// A handler that takes two seconds of (virtual) time.
    async fn slow() -> &'static str {
        tokio::time::sleep(Duration::from_secs(2)).await;
        "done"
    }

    #[tokio::test(start_paused = true)]
    async fn a_handler_past_the_deadline_is_answered_408_in_the_error_shape() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 1,
            spawn_timeout_secs: 330,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let app = app(limits, Router::new().route("/slow", get(slow)));
        let response = app.oneshot(request("/slow")).await.unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(error_of(response).await, "request took longer than 1 s");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_of_zero_lets_a_slow_handler_finish() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 0,
            spawn_timeout_secs: 0,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let app = app(limits, Router::new().route("/slow", get(slow)));
        let response = app.oneshot(request("/slow")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn a_handler_inside_the_deadline_is_untouched() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 5,
            spawn_timeout_secs: 330,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let app = app(limits, Router::new().route("/slow", get(slow)));
        let response = app.oneshot(request("/slow")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The websocket routes are what a client keeps open on purpose. Both
    /// shapes: the global feed and a per-run one.
    #[tokio::test(start_paused = true)]
    async fn the_websocket_routes_outlive_the_deadline() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 1,
            spawn_timeout_secs: 330,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let router = Router::new()
            .route("/ws", get(slow))
            .route("/ws/agents/{id}", get(slow))
            .route("/wsx", get(slow));
        for path in ["/ws", "/ws/agents/run-1"] {
            let response = app(limits, router.clone())
                .oneshot(request(path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        // A path that merely starts with the letters is not a websocket.
        let response = app(limits, router).oneshot(request("/wsx")).await.unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    /// The routes that legitimately outlive the deadline: an OAuth login waits
    /// on a person at a consent page, an MCP test on a server's handshake, a
    /// live doctor on provider calls and a spawned run. Each finishes under a
    /// deadline it would otherwise have blown, and a lookalike path does not.
    #[tokio::test(start_paused = true)]
    async fn the_long_routes_outlive_the_deadline() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 1,
            spawn_timeout_secs: 330,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let router = Router::new()
            .route("/api/mcp/servers/{name}/login", post(slow))
            .route("/api/mcp/servers/{name}/test", post(slow))
            .route("/api/doctor/live", post(slow))
            .route("/api/doctor", get(slow))
            .route("/api/mcp/servers/{name}/status", get(slow));
        for path in [
            "/api/mcp/servers/navigator/login",
            "/api/mcp/servers/navigator/test",
            "/api/doctor/live",
        ] {
            let response = app(limits, router.clone())
                .oneshot(post_request(path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        // The offline doctor and a status read are ordinary routes.
        for path in ["/api/doctor", "/api/mcp/servers/navigator/status"] {
            let response = app(limits, router.clone())
                .oneshot(request(path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT, "{path}");
        }
    }

    /// The spawn routes and `/graphql` keep their own deadline, so the
    /// layer's does not cut them off; the same paths under another method
    /// are ordinary routes.
    #[tokio::test(start_paused = true)]
    async fn the_routes_with_their_own_deadline_outlive_the_layers() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 1,
            spawn_timeout_secs: 330,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let router = Router::new()
            .route("/api/runs", post(slow).get(slow))
            .route("/api/runs/validate", post(slow))
            .route("/graphql", post(slow).get(slow));
        for path in ["/api/runs", "/api/runs/validate", "/graphql"] {
            let response = app(limits, router.clone())
                .oneshot(post_request(path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        for path in ["/api/runs", "/graphql"] {
            let response = app(limits, router.clone())
                .oneshot(request(path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT, "{path}");
        }
    }

    /// Work inside its deadline answers, work past it does not, and no
    /// deadline waits for it.
    #[tokio::test(start_paused = true)]
    async fn within_cuts_work_off_at_its_deadline_and_only_then() {
        assert_eq!(within(Some(Duration::from_secs(1)), slow()).await, None);
        assert_eq!(
            within(Some(Duration::from_secs(5)), slow()).await,
            Some("done")
        );
        assert_eq!(within(None, slow()).await, Some("done"));
    }

    /// A route outside the deadline is still inside the cap: a parked login
    /// holds the only permit, so the next request is refused.
    #[tokio::test]
    async fn a_deadline_exempt_route_still_takes_a_permit() {
        let limits = RequestLimits {
            max_concurrent_requests: 1,
            request_timeout_secs: 1,
            spawn_timeout_secs: 330,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let parked = Arc::new(Parked::default());
        let app = app(
            limits,
            Router::new()
                .route("/api/mcp/servers/{name}/login", post(super::tests::parked))
                .route("/api/runs", get(super::tests::parked))
                .with_state(Arc::clone(&parked)),
        );
        let login = {
            let app = app.clone();
            tokio::spawn(async move {
                app.oneshot(post_request("/api/mcp/servers/navigator/login"))
                    .await
                    .unwrap()
            })
        };
        while parked.arrived.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        let refused = app.clone().oneshot(request("/api/runs")).await.unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error_of(refused).await,
            "too many requests in flight (the cap is 1)"
        );
        parked.release.notify_waiters();
        assert_eq!(login.await.unwrap().status(), StatusCode::OK);
        // The permit came back with the response.
        parked.release.notify_one();
        let after = app.oneshot(request("/api/runs")).await.unwrap();
        assert_eq!(after.status(), StatusCode::OK);
    }

    /// The pattern matcher, segment by segment: `*` stands for one segment,
    /// never zero and never several, and nothing matches a prefix.
    #[test]
    fn an_exempt_pattern_matches_one_segment_per_star() {
        assert!(matches_route(
            "/api/mcp/servers/*/login",
            "/api/mcp/servers/a/login"
        ));
        assert!(!matches_route(
            "/api/mcp/servers/*/login",
            "/api/mcp/servers//login"
        ));
        assert!(!matches_route(
            "/api/mcp/servers/*/login",
            "/api/mcp/servers/a/b/login"
        ));
        assert!(!matches_route(
            "/api/mcp/servers/*/login",
            "/api/mcp/servers/a/login/x"
        ));
        assert!(!matches_route("/api/doctor/live", "/api/doctor"));
        assert!(!matches_route("/api/doctor/live", "/api/doctor/liver"));
        assert!(matches_route("/api/doctor/live", "/api/doctor/live"));
    }

    /// A handler that parks until the test releases it, counting arrivals so
    /// the test knows when every admitted request is truly in flight.
    #[derive(Default)]
    struct Parked {
        arrived: AtomicUsize,
        release: Notify,
    }

    async fn parked(State(parked): State<Arc<Parked>>) -> &'static str {
        parked.arrived.fetch_add(1, Ordering::SeqCst);
        parked.release.notified().await;
        "released"
    }

    /// Sixty-five requests against a cap of sixty-four: exactly one is
    /// refused, and it is refused while the others are still running rather
    /// than after.
    #[tokio::test]
    async fn one_request_over_the_cap_is_answered_503() {
        let limits = RequestLimits {
            max_concurrent_requests: 64,
            request_timeout_secs: 0,
            spawn_timeout_secs: 0,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let parked = Arc::new(Parked::default());
        let app = app(
            limits,
            Router::new()
                .route("/park", get(super::tests::parked))
                .with_state(Arc::clone(&parked)),
        );

        let mut admitted = Vec::new();
        for _ in 0..64 {
            let app = app.clone();
            admitted.push(tokio::spawn(async move {
                app.oneshot(request("/park")).await.unwrap()
            }));
        }
        while parked.arrived.load(Ordering::SeqCst) < 64 {
            tokio::task::yield_now().await;
        }

        // Bounded, so a router that admits the 65th parks it and fails here
        // rather than hanging the test binary.
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(request("/park")),
        )
        .await
        .expect("the 65th request is answered, not parked")
        .unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error_of(refused).await,
            "too many requests in flight (the cap is 64)"
        );

        parked.release.notify_waiters();
        for task in admitted {
            assert_eq!(task.await.unwrap().status(), StatusCode::OK);
        }
        assert_eq!(parked.arrived.load(Ordering::SeqCst), 64);

        // Every permit came back: the next request is admitted again.
        parked.release.notify_one();
        let after = app.oneshot(request("/park")).await.unwrap();
        assert_eq!(after.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_cap_of_zero_admits_everything() {
        let limits = RequestLimits {
            max_concurrent_requests: 0,
            request_timeout_secs: 0,
            spawn_timeout_secs: 0,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        let parked = Arc::new(Parked::default());
        let app = app(
            limits,
            Router::new()
                .route("/park", get(super::tests::parked))
                .with_state(Arc::clone(&parked)),
        );
        let mut tasks = Vec::new();
        for _ in 0..70 {
            let app = app.clone();
            tasks.push(tokio::spawn(async move {
                app.oneshot(request("/park")).await.unwrap()
            }));
        }
        while parked.arrived.load(Ordering::SeqCst) < 70 {
            tokio::task::yield_now().await;
        }
        parked.release.notify_waiters();
        for task in tasks {
            assert_eq!(task.await.unwrap().status(), StatusCode::OK);
        }
    }

    #[test]
    fn a_flag_beats_the_config_and_the_config_beats_the_default() {
        let config = ServeConfig {
            max_concurrent_requests: 8,
            request_timeout_secs: 120,
            spawn_timeout_secs: 600,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
            event_replay_frames: crate::config::DEFAULT_EVENT_REPLAY_FRAMES,
        };
        let limits =
            |max_concurrent_requests, request_timeout_secs, spawn_timeout_secs| RequestLimits {
                max_concurrent_requests,
                request_timeout_secs,
                spawn_timeout_secs,
                max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
            };
        // No flag: the config stands.
        assert_eq!(
            RequestLimits::resolve(Flags::default(), &config),
            limits(8, 120, 600)
        );
        // Each flag on its own, including `0` to switch a limit off.
        let cap = Flags {
            max_concurrent_requests: Some(0),
            ..Flags::default()
        };
        assert_eq!(RequestLimits::resolve(cap, &config), limits(0, 120, 600));
        let timeout = Flags {
            request_timeout_secs: Some(3),
            ..Flags::default()
        };
        assert_eq!(RequestLimits::resolve(timeout, &config), limits(8, 3, 600));
        let spawn = Flags {
            spawn_timeout_secs: Some(0),
            ..Flags::default()
        };
        assert_eq!(RequestLimits::resolve(spawn, &config), limits(8, 120, 0));
        // No config table and no flags: the defaults.
        assert_eq!(
            RequestLimits::resolve(Flags::default(), &ServeConfig::default()),
            RequestLimits::default()
        );
        assert_eq!(RequestLimits::default().max_concurrent_requests, 64);
        assert_eq!(RequestLimits::default().request_timeout_secs, 30);
        assert_eq!(RequestLimits::default().spawn_timeout_secs, 330);
        assert_eq!(limits(8, 120, 0).spawn_timeout(), None);
        assert_eq!(
            limits(8, 120, 600).spawn_timeout(),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn a_cap_beyond_the_platform_is_no_cap() {
        let limits = RequestLimits {
            max_concurrent_requests: u64::MAX,
            request_timeout_secs: 0,
            spawn_timeout_secs: 0,
            max_upload_bytes: crate::config::DEFAULT_MAX_UPLOAD_BYTES,
        };
        assert_eq!(limits.cap(), Some(usize::MAX));
        let gate = Gate::new(limits);
        assert_eq!(
            gate.in_flight.as_ref().map(|s| s.available_permits()),
            Some(Semaphore::MAX_PERMITS)
        );
    }

    /// A limits record written before the upload ceiling existed still reads,
    /// with the default in that slot.
    #[test]
    fn an_older_limits_record_reads_the_default_upload_ceiling() {
        let limits: RequestLimits = serde_json::from_value(serde_json::json!({
            "max_concurrent_requests": 4,
            "request_timeout_secs": 9
        }))
        .unwrap();
        assert_eq!(limits.max_concurrent_requests, 4);
        assert_eq!(
            limits.max_upload_bytes,
            crate::config::DEFAULT_MAX_UPLOAD_BYTES
        );
        assert_eq!(limits.spawn_timeout_secs, 330);
    }
}
