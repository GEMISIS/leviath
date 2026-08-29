//! How many requests `lev serve` takes on at once, and how long it gives each.
//!
//! Before this the router had an auth layer, an optional CORS layer, and
//! axum's own 2 MiB body limit, and nothing else: a client that opened a
//! thousand connections held a thousand handlers, and a route that never
//! answered held its connection for as long as the client cared to wait.
//!
//! Two ceilings, both configurable in the config file and on the command line
//! and both switched off with `0`:
//!
//! - a cap on requests in flight, over which the next request is answered
//!   `503` at once rather than queued, so a flood is refused instead of
//!   piling up behind whatever is slow;
//! - a timeout per request, after which the handler is dropped and the client
//!   gets `408`.
//!
//! The websocket routes are outside both. A subscription is meant to stay open
//! for hours, and it is the one place the API streams: every other route
//! builds its whole body before answering, so cutting a handler at the deadline
//! never cuts a body in half.

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
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_concurrent_requests: crate::config::DEFAULT_MAX_CONCURRENT_REQUESTS,
            request_timeout_secs: crate::config::DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

impl RequestLimits {
    /// Reconcile a flag with its config key: a flag that was typed wins, a
    /// config value stands otherwise, and the config's own default fills in
    /// when neither was set.
    pub(super) fn resolve(
        flag_max_concurrent: Option<u64>,
        flag_timeout_secs: Option<u64>,
        config: &crate::config::ServeConfig,
    ) -> Self {
        Self {
            max_concurrent_requests: flag_max_concurrent.unwrap_or(config.max_concurrent_requests),
            request_timeout_secs: flag_timeout_secs.unwrap_or(config.request_timeout_secs),
        }
    }

    /// The per-request deadline, or `None` when it is switched off.
    fn timeout(&self) -> Option<Duration> {
        (self.request_timeout_secs > 0).then(|| Duration::from_secs(self.request_timeout_secs))
    }

    /// The in-flight cap, or `None` when it is switched off.
    fn cap(&self) -> Option<usize> {
        // A cap larger than the platform's `usize` is no cap at all.
        (self.max_concurrent_requests > 0)
            .then(|| usize::try_from(self.max_concurrent_requests).unwrap_or(usize::MAX))
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

/// Whether a path is one the limits leave alone: the websocket routes, which
/// hold their connection open by design.
fn is_exempt(path: &str) -> bool {
    path == "/ws" || path.starts_with("/ws/")
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

    // Held until the response is built, then released. The body is sent after
    // that, which is fine: nothing but the websocket routes streams one.
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

    match gate.limits.timeout() {
        None => next.run(req).await,
        Some(deadline) => match tokio::time::timeout(deadline, next.run(req)).await {
            Ok(response) => response,
            Err(_) => err(
                StatusCode::REQUEST_TIMEOUT,
                format!(
                    "request took longer than {} s",
                    gate.limits.request_timeout_secs
                ),
            )
            .into_response(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
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
        };
        // Neither flag: the config stands.
        assert_eq!(
            RequestLimits::resolve(None, None, &config),
            RequestLimits {
                max_concurrent_requests: 8,
                request_timeout_secs: 120,
            }
        );
        // Each flag on its own, including `0` to switch a limit off.
        assert_eq!(
            RequestLimits::resolve(Some(0), None, &config),
            RequestLimits {
                max_concurrent_requests: 0,
                request_timeout_secs: 120,
            }
        );
        assert_eq!(
            RequestLimits::resolve(None, Some(3), &config),
            RequestLimits {
                max_concurrent_requests: 8,
                request_timeout_secs: 3,
            }
        );
        // No config table and no flags: the defaults.
        assert_eq!(
            RequestLimits::resolve(None, None, &ServeConfig::default()),
            RequestLimits::default()
        );
        assert_eq!(RequestLimits::default().max_concurrent_requests, 64);
        assert_eq!(RequestLimits::default().request_timeout_secs, 30);
    }

    #[test]
    fn a_cap_beyond_the_platform_is_no_cap() {
        let limits = RequestLimits {
            max_concurrent_requests: u64::MAX,
            request_timeout_secs: 0,
        };
        assert_eq!(limits.cap(), Some(usize::MAX));
        let gate = Gate::new(limits);
        assert_eq!(
            gate.in_flight.as_ref().map(|s| s.available_permits()),
            Some(Semaphore::MAX_PERMITS)
        );
    }
}
