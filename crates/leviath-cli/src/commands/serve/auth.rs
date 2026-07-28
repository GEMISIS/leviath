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
/// environment variable. Returns an error — refusing to start — when neither is
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

/// The token a request presents: `Authorization: Bearer <token>`, else the
/// `token` query parameter (for WebSocket clients that can't set headers).
fn presented_token(req: &Request) -> Option<String> {
    if let Some(bearer) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        return Some(bearer.trim().to_string());
    }
    req.uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .map(|t| t.to_string())
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
        let uri = match query {
            Some(q) => format!("http://x/api/agents?{q}"),
            None => "http://x/api/agents".to_string(),
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
    fn presented_token_reads_header_then_query() {
        assert_eq!(
            presented_token(&req(Some("Bearer tok123"), None)).as_deref(),
            Some("tok123")
        );
        // Non-bearer header ⇒ falls to query.
        assert_eq!(
            presented_token(&req(Some("Basic x"), Some("token=qtok"))).as_deref(),
            Some("qtok")
        );
        assert_eq!(
            presented_token(&req(None, Some("foo=1&token=qtok"))).as_deref(),
            Some("qtok")
        );
        assert!(presented_token(&req(None, None)).is_none());
        // A different key that ends in "token" isn't mistaken for it.
        assert!(presented_token(&req(None, Some("mytoken=x"))).is_none());
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

        // Valid via query param ⇒ 200.
        let okq = app
            .clone()
            .oneshot(req(None, Some("token=secret")))
            .await
            .unwrap();
        assert_eq!(okq.status(), StatusCode::OK);

        // Missing token ⇒ 401.
        let missing = app.clone().oneshot(req(None, None)).await.unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        // Wrong token ⇒ 401.
        let wrong = app.oneshot(req(Some("Bearer nope"), None)).await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
}
