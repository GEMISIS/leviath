//! Tests for the admin gate: how it is worked out at startup, what it admits,
//! and that every admin route answers to it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use tower::ServiceExt;

use super::{ADMIN_TOKEN_ENV, ADMIN_TOKEN_HEADER, ADMIN_TOKEN_REQUIRED, AdminGate, Presented};

/// The gate's variant, as a word an assertion can compare.
fn kind(gate: &AdminGate) -> String {
    match gate {
        AdminGate::Off => "off".to_string(),
        AdminGate::Open => "open".to_string(),
        AdminGate::Token(token) => format!("token:{token}"),
    }
}

/// `resolve` under a given environment, as the word [`kind`] gives, or the
/// refusal's message.
fn resolved(env: Option<&str>, allow_admin: bool, arg: Option<&str>) -> String {
    temp_env::with_var(ADMIN_TOKEN_ENV, env, || {
        AdminGate::resolve(allow_admin, arg, "api")
            .map(|gate| kind(&gate))
            .unwrap_or_else(|refused| refused.to_string())
    })
}

/// With no admin token, the gate is what it has always been: off by default,
/// open to the API token with `--allow-admin`.
#[test]
fn without_an_admin_token_the_flag_decides() {
    assert_eq!(resolved(None, false, None), "off");
    assert_eq!(resolved(None, true, None), "open");
}

/// An admin token from the flag or the environment narrows the gate, and it
/// wins over `--allow-admin`: naming one never opens the routes to the API
/// token as well. The flag wins over the environment, as `--token` does.
#[test]
fn an_admin_token_narrows_the_gate_whichever_way_it_arrives() {
    assert_eq!(resolved(Some("root"), false, None), "token:root");
    assert_eq!(resolved(Some(" root "), true, None), "token:root");
    assert_eq!(resolved(None, true, Some("root")), "token:root");
    assert_eq!(resolved(Some("env"), false, Some("flag")), "token:flag");
}

/// A blank admin token, or one that is the API token, refuses to start rather
/// than falling back to something broader than was asked for.
#[test]
fn a_blank_or_reused_admin_token_refuses_to_start() {
    for (env, arg) in [(Some("  "), None), (None, Some(""))] {
        let refused = resolved(env, true, arg);
        assert!(refused.contains("set but empty"), "{refused}");
    }
    let refused = resolved(None, false, Some("api"));
    assert!(refused.contains("must differ"), "{refused}");
}

/// What a request with this bearer and this admin header presents, on `path`.
fn presented(path: &str, bearer: Option<&str>, header: Option<&str>) -> Presented {
    let mut headers = HeaderMap::new();
    if let Some(bearer) = bearer {
        headers.insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    }
    if let Some(header) = header {
        headers.insert(ADMIN_TOKEN_HEADER, header.parse().unwrap());
    }
    Presented::of(&headers, &path.parse::<Uri>().unwrap())
}

/// Off admits nobody and open admits everybody, whatever they present.
#[test]
fn off_and_open_ignore_what_a_request_presents() {
    for request in [
        Presented::default(),
        presented("/api/config", Some("root"), Some("root")),
    ] {
        assert!(!AdminGate::Off.admits(&request));
        assert!(AdminGate::Open.admits(&request));
    }
}

/// A token gate admits the admin token as the bearer, in the header beside the
/// API token, or as a WebSocket's `?token=`; and nothing else.
#[test]
fn a_token_gate_admits_only_the_admin_token() {
    let gate = AdminGate::Token(Arc::new("root".to_string()));
    for (request, admitted) in [
        (presented("/api/config", Some("root"), None), true),
        (presented("/api/config", Some("api"), Some("root")), true),
        (presented("/api/config", Some("api"), Some(" root ")), true),
        (presented("/ws/graphql?token=root", None, None), true),
        (presented("/api/config", Some("api"), None), false),
        (presented("/api/config", Some("api"), Some("nope")), false),
        (presented("/api/config?token=root", None, None), false),
        (Presented::default(), false),
    ] {
        assert_eq!(gate.admits(&request), admitted);
    }
}

/// A header value that is not text presents nothing, rather than failing.
#[test]
fn an_unreadable_admin_header_presents_nothing() {
    let mut headers = HeaderMap::new();
    headers.insert(
        ADMIN_TOKEN_HEADER,
        axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    let gate = AdminGate::Token(Arc::new("root".to_string()));
    assert!(!gate.admits(&Presented::of(&headers, &Uri::from_static("/api/config"))));
}

/// Only a token gate has a token to authenticate with, and only off hides the
/// admin surface.
#[test]
fn the_gate_reports_its_token_and_whether_it_is_mounted() {
    let gate = AdminGate::Token(Arc::new("root".to_string()));
    assert_eq!(gate.token().as_deref().map(String::as_str), Some("root"));
    assert!(AdminGate::Open.token().is_none());
    assert!(AdminGate::Off.token().is_none());
    assert!(gate.mounted());
    assert!(AdminGate::Open.mounted());
    assert!(!AdminGate::Off.mounted());
}

/// Every route `admin_routes` declares, with its path parameters filled in.
fn declared_admin_routes() -> Vec<(String, String)> {
    let source = super::super::production_source();
    let (_, body) = source
        .split_once("fn admin_routes() -> Router<AppState> {")
        .expect("admin_routes is declared");
    let (body, _) = body.split_once("\n}\n").expect("and it ends");
    super::super::routes_in(body)
}

/// The admin routes as `execute_with_shutdown` mounts them, behind `gate`.
fn admin_app(gate: AdminGate) -> axum::Router {
    super::super::admin_routes()
        .route_layer(axum::middleware::from_fn_with_state(
            gate,
            super::require_admin,
        ))
        .with_state(super::super::testutil::state_with_agent_paths(Vec::new()))
}

/// One request to `path`, with these credentials.
fn request(method: &str, path: &str, bearer: &str, header: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(header) = header {
        builder = builder.header(ADMIN_TOKEN_HEADER, header);
    }
    builder.body(Body::empty()).unwrap()
}

/// Every admin route refuses a request that does not present the admin token,
/// on a server that has one. Read off the route table itself, so a route added
/// to `admin_routes` is held to this without anyone listing it here.
///
/// The refusal is the gate's, before the handler: it names where the token
/// goes, never the token.
#[tokio::test]
async fn every_admin_route_refuses_a_request_without_the_admin_token() {
    let routes = declared_admin_routes();
    assert!(routes.len() > 10, "{routes:?}");
    let app = admin_app(AdminGate::Token(Arc::new("root".to_string())));
    for (path, method) in &routes {
        let path = path.replace(['{', '}'], "");
        for header in [None, Some("nope")] {
            let resp = app
                .clone()
                .oneshot(request(method, &path, "api", header))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {path}");
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let body = String::from_utf8_lossy(&body).to_string();
            assert!(body.contains(ADMIN_TOKEN_REQUIRED), "{body}");
            assert!(!body.contains("root"), "{body}");
        }
    }
}

/// The admin token reaches the handler, as the bearer or in the header; and an
/// open gate lets the API token through, as `--allow-admin` always has. The
/// update job route is the probe because its handler touches nothing: a job
/// nobody started is a 404 from the handler, not a 403 from the gate.
#[tokio::test]
async fn the_admin_token_or_an_open_gate_reaches_the_handler() {
    let gated = admin_app(AdminGate::Token(Arc::new("root".to_string())));
    let open = admin_app(AdminGate::Open);
    for (app, bearer, header) in [
        (gated.clone(), "root", None),
        (gated, "api", Some("root")),
        (open, "api", None),
    ] {
        let resp = app
            .oneshot(request("GET", "/api/update/jobs/nope", bearer, header))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

/// The spec marks exactly the routes `admin_routes` mounts as admin, and
/// documents the gate's `403` on each. A route documented as admin but mounted
/// in the open table, or the reverse, fails here.
#[test]
fn the_spec_marks_exactly_the_admin_routes() {
    let spec: serde_json::Value =
        serde_json::from_str(include_str!("../../../../../docs/schema/openapi.json"))
            .expect("the spec is JSON");
    let mut marked: Vec<(String, String)> = spec["paths"]
        .as_object()
        .expect("paths")
        .iter()
        .flat_map(|(path, operations)| {
            operations
                .as_object()
                .expect("operations")
                .iter()
                .filter(|(_, operation)| operation["x-leviath-admin"] == true)
                .map(|(method, operation)| {
                    assert!(
                        operation["responses"]["403"].is_object(),
                        "{method} {path} documents the gate's 403"
                    );
                    (path.clone(), method.to_uppercase())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut declared = declared_admin_routes();
    marked.sort();
    declared.sort();
    assert_eq!(marked, declared);
}
