//! Bearer-token authentication for the API server.
//!
//! The API can spawn tool-/shell-executing agents, so it must never run
//! unauthenticated. [`resolve_token`] refuses to start the server unless a token
//! is configured (via `--token` or `LEVIATH_API_TOKEN`), and [`require_auth`] is
//! a middleware that rejects any request without a matching token. Clients send
//! it as `Authorization: Bearer <token>`; WebSocket clients (browsers can't set
//! request headers on a WS upgrade) may instead pass `?token=<token>`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Resolve the API token from `--token` (`arg`) or the `LEVIATH_API_TOKEN`
/// environment variable. Returns an error - refusing to start - when neither is
/// set, so an unauthenticated agent-spawning API can never be launched.
pub(super) fn resolve_token(arg: Option<&str>) -> anyhow::Result<String> {
    if let Some(t) = arg.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    if let Ok(env) = std::env::var("LEVIATH_API_TOKEN") {
        let t = env.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    anyhow::bail!(
        "refusing to start an unauthenticated API server: it can spawn \
         tool-executing agents. Set a token with `--token <token>` or the \
         LEVIATH_API_TOKEN environment variable. Clients send it as \
         `Authorization: Bearer <token>` (or `?token=<token>` for WebSockets)."
    )
}

/// Constant-time string equality, so a wrong token can't be recovered by timing.
///
/// Re-exported from `leviath_core::secrets` rather than kept as a local copy, so
/// there is one such comparison in the workspace. The MCP OAuth callback's
/// `state` check used plain `==` until it was pointed at this one.
use leviath_core::constant_time_eq;

/// The token a request presents: `Authorization: Bearer <token>`, else - **on
/// the WebSocket routes only** - the `token` query parameter.
///
/// The query form exists because a browser cannot set request headers on a
/// WebSocket upgrade. Accepting it on *every* route would let an ordinary REST
/// call authenticate with the token in its URL - and a URL ends up in
/// reverse-proxy access logs, browser history, and `Referer` headers on any
/// outbound link. Restricting it to the routes that genuinely cannot use a
/// header keeps the escape hatch without spreading the credential.
fn presented_token(req: &Request) -> Option<String> {
    if let Some(bearer) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        return Some(bearer.trim().to_string());
    }
    if !is_websocket_route(req.uri().path()) {
        return None;
    }
    req.uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .map(|t| t.to_string())
}

/// Whether `path` is one of the WebSocket upgrade routes (`/ws`, `/ws/...`).
fn is_websocket_route(path: &str) -> bool {
    path == "/ws" || path.starts_with("/ws/")
}

/// Middleware: allow the request through only when it presents the expected
/// token; otherwise respond `401 Unauthorized`.
pub(super) async fn require_auth(
    State(expected): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Response {
    match presented_token(&req) {
        Some(token) if constant_time_eq(&token, &expected) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            "unauthorized: missing or invalid API token",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    fn req(auth: Option<&str>, query: Option<&str>) -> Request {
        req_to("/api/agents", auth, query)
    }

    /// The `?token=` escape hatch is scoped to the WebSocket routes, so tests
    /// have to say which path they are exercising.
    fn req_to(path: &str, auth: Option<&str>, query: Option<&str>) -> Request {
        let uri = match query {
            Some(q) => format!("http://x{path}?{q}"),
            None => format!("http://x{path}"),
        };
        let mut b = HttpRequest::builder().uri(uri);
        if let Some(a) = auth {
            b = b.header(AUTHORIZATION, a);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn resolve_token_prefers_arg_then_env_then_errors() {
        assert_eq!(resolve_token(Some("from-arg")).unwrap(), "from-arg");
        assert_eq!(resolve_token(Some("  spaced  ")).unwrap(), "spaced");
        // Blank arg falls through to env.
        temp_env::with_var("LEVIATH_API_TOKEN", Some("from-env"), || {
            assert_eq!(resolve_token(Some("   ")).unwrap(), "from-env");
            assert_eq!(resolve_token(None).unwrap(), "from-env");
        });
        // Blank env is treated as unset.
        temp_env::with_var("LEVIATH_API_TOKEN", Some("  "), || {
            assert!(resolve_token(None).is_err());
        });
        temp_env::with_var("LEVIATH_API_TOKEN", None::<&str>, || {
            assert!(resolve_token(None).is_err());
        });
    }

    #[test]
    fn constant_time_eq_matches_std_equality() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd")); // length mismatch
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn presented_token_reads_the_bearer_header_on_any_route() {
        assert_eq!(
            presented_token(&req(Some("Bearer tok123"), None)).as_deref(),
            Some("tok123")
        );
        assert!(presented_token(&req(None, None)).is_none());
    }

    /// A browser cannot set headers on a WebSocket upgrade, so `?token=` is
    /// accepted there.
    #[test]
    fn the_query_token_works_on_the_websocket_routes() {
        for path in ["/ws", "/ws/agents/run-1"] {
            assert_eq!(
                presented_token(&req_to(path, None, Some("token=qtok"))).as_deref(),
                Some("qtok"),
                "{path}"
            );
            assert_eq!(
                presented_token(&req_to(path, None, Some("foo=1&token=qtok"))).as_deref(),
                Some("qtok"),
                "{path}"
            );
            // Non-bearer header ⇒ falls through to the query.
            assert_eq!(
                presented_token(&req_to(path, Some("Basic x"), Some("token=qtok"))).as_deref(),
                Some("qtok"),
                "{path}"
            );
            // A different key that merely ends in "token" is not mistaken for it.
            assert!(presented_token(&req_to(path, None, Some("mytoken=x"))).is_none());
        }
    }

    /// ...and nowhere else. A URL carrying the token ends up in reverse-proxy
    /// access logs, browser history and `Referer` headers; a REST client can set
    /// a header, so it has no reason to put the credential there.
    #[test]
    fn the_query_token_is_refused_on_ordinary_routes() {
        for path in ["/api/agents", "/api/config", "/wsomething", "/"] {
            assert!(
                presented_token(&req_to(path, None, Some("token=qtok"))).is_none(),
                "{path} must not accept a query token"
            );
        }
    }

    #[tokio::test]
    async fn require_auth_allows_valid_and_rejects_invalid() {
        use axum::routing::get;
        use axum::{Router, middleware};
        use tower::ServiceExt;

        let expected = Arc::new("secret".to_string());
        let app: Router = Router::new()
            .route("/api/agents", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(expected, require_auth));

        // Valid bearer token ⇒ 200.
        let ok = app
            .clone()
            .oneshot(req(Some("Bearer secret"), None))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        // The right token in the query string, on a non-WebSocket route ⇒ 401.
        // The credential does not belong in a URL where a header will do.
        let query_on_rest = app
            .clone()
            .oneshot(req(None, Some("token=secret")))
            .await
            .unwrap();
        assert_eq!(query_on_rest.status(), StatusCode::UNAUTHORIZED);

        // Missing token ⇒ 401.
        let missing = app.clone().oneshot(req(None, None)).await.unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        // Wrong token ⇒ 401.
        let wrong = app.oneshot(req(Some("Bearer nope"), None)).await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
}
