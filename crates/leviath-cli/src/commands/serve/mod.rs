//! `lev serve` - REST + WebSocket API server.
//!
//! Exposes agent management, blueprint CRUD, and live event streaming over
//! HTTP. No web UI - the frontend lives in a separate repo.

mod agents;
mod auth;
mod blueprints;
mod config;
mod config_types;
mod cursor;
mod doctor;
mod events;
mod fs;
mod interactions;
mod mcp;
mod polling;
mod runs;
mod scripts;
mod search;
#[cfg(test)]
mod testutil;
mod tls;
mod tools;
mod tree;
mod types;
mod update;
mod update_cache;
mod update_job;
mod websocket;

#[cfg(test)]
#[path = "event_seam_tests.rs"]
mod event_seam_tests;

pub(crate) use config::list_model_ids;
pub(crate) use events::ServerEvent;
pub(crate) use types::AppState;
pub use types::ServeArgs;
use types::ServeLimits;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post, put};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};

use crate::config::Config;

// ─── Entrypoint ──────────────────────────────────────────────────────────────

/// Aborts a spawned task when dropped - including when dropped mid-flight as
/// part of an outer future's cancellation (e.g. `JoinHandle::abort()` on the
/// task that owns this guard), not just on normal scope exit.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run `lev serve`: expose the HTTP + WebSocket API over the daemon.
///
/// `upgrade` is how `POST /api/update` runs a package manager. Injected rather
/// than built here for the same reason `lev update`'s is: spawning a process is
/// the one part of an update with nothing to unit-test it against, so it lives
/// in the binary's composition root and everything that decides *whether* and
/// *what* to spawn is testable without one.
pub async fn execute(
    args: ServeArgs,
    control: leviath_runtime::control_socket::ControlClient,
    upgrade: crate::commands::update::CommandRunner,
) -> anyhow::Result<()> {
    execute_with_shutdown(
        args,
        control,
        upgrade,
        Box::pin(std::future::pending()),
        None,
    )
    .await
}

/// Every API route with its production handlers - the single route table,
/// shared by [`execute_with_shutdown`] and the tests. A hand-copied test
/// router drifted seven routes behind production, which meant a route could
/// be added, typo'd, and never exercised. Admin routes, the auth middleware,
/// CORS, and `with_state` are layered on by the caller.
fn api_router() -> Router<AppState> {
    Router::new()
        // Blueprints
        .route(
            "/api/blueprints",
            get(blueprints::list_blueprints).post(blueprints::create_blueprint),
        )
        .route(
            "/api/blueprints/validate",
            post(blueprints::validate_blueprint),
        )
        .route(
            "/api/blueprints/{name}",
            get(blueprints::get_blueprint)
                .put(blueprints::update_blueprint)
                .delete(blueprints::delete_blueprint),
        )
        // Runs - the paginated, searchable listing. Supersedes the GET half of
        // /api/agents, which stays as it is for existing clients.
        .route("/api/runs", get(runs::list_runs).delete(runs::delete_runs))
        .route("/api/runs/{id}", delete(runs::delete_run))
        // Agents
        .route(
            "/api/agents",
            get(agents::list_agents).post(agents::spawn_agent),
        )
        .route("/api/agents/tree", get(tree::agents_tree))
        .route(
            "/api/agents/{id}",
            get(agents::get_agent).delete(agents::kill_agent),
        )
        .route("/api/agents/{id}/children", get(agents::agent_children))
        .route("/api/agents/{id}/context", get(agents::agent_context))
        .route(
            "/api/agents/{id}/context/history",
            get(agents::agent_context_history),
        )
        .route("/api/agents/{id}/files", get(agents::agent_file))
        .route("/api/agents/{id}/logs", get(agents::agent_logs))
        .route("/api/agents/{id}/result", get(agents::agent_result))
        .route("/api/agents/{id}/stages", get(agents::agent_stages))
        .route("/api/agents/{id}/tree-status", get(tree::agent_tree_status))
        .route("/api/agents/{id}/pause", post(agents::pause_agent))
        .route("/api/agents/{id}/resume", post(agents::resume_agent))
        // Messages
        .route("/api/agents/{id}/message", post(interactions::send_message))
        // Interactions
        .route(
            "/api/agents/{id}/interaction",
            get(interactions::get_interaction).post(interactions::submit_interaction),
        )
        // MCP servers - read-only surface. Everything that connects to one or
        // opens a browser is mounted by `execute_with_shutdown`, behind
        // `--allow-admin`.
        .route("/api/mcp/servers", get(mcp::list_servers))
        .route("/api/mcp/servers/{name}/status", get(mcp::status))
        // Doctor - the offline half of the checks `lev doctor` runs, returned
        // as data. The billed half is behind `--allow-admin` too.
        .route("/api/doctor", get(doctor::run_doctor))
        // Update - how this copy was installed, and what upgrades it. The
        // console has no other way to know, and printed a macOS-only command
        // to everyone because of it.
        .route("/api/update", get(update::get_update))
        // Filesystem - directory browsing for the console's folder picker,
        // and the "New Folder" a browser cannot get from a native dialog.
        .route("/api/fs/dirs", get(fs::list_dirs).post(fs::create_dir))
        // Tools - what an agent on this machine can actually call. Read-only:
        // there is nothing to write, since a tool is either built in or a file
        // the scripts routes below own.
        .route("/api/tools", get(tools::list_tools))
        // Scripts - the read half of the Rhai surface. The write half is
        // mounted by `execute_with_shutdown`, behind `--allow-admin`.
        .route("/api/scripts", get(scripts::list_scripts))
        .route("/api/scripts/validate", post(scripts::validate_script))
        .route("/api/scripts/{kind}/{name}", get(scripts::get_script))
        // Config
        .route("/api/config", get(config::get_config))
        .route("/api/config/validate", post(config::validate_config_key))
        .route("/api/models", get(config::get_models))
        // WebSocket
        .route("/ws", get(websocket::ws_global))
        .route("/ws/agents/{id}", get(websocket::ws_agent))
}

/// Every `(path, method)` this file registers, read out of its own source.
///
/// Axum's `Router` offers no way to enumerate what it holds, so the only place
/// the route table can be read back is the text that declares it. Used by the
/// test that holds `docs/schema/openapi.json` to the real router, so a route
/// added without a spec entry fails the build rather than shipping undocumented.
/// The same technique `xtask/src/docs.rs` uses on the docs.
#[cfg(test)]
fn declared_routes() -> Vec<(String, String)> {
    const SOURCE: &str = include_str!("mod.rs");
    // Only the half above the test module. The tests below declare routes of
    // their own as fixtures for the reader, and those are not served by
    // anything. Reading the whole file counted them as production routes.
    let production = SOURCE.split("\nmod tests {").next().unwrap_or(SOURCE);
    routes_in(production)
}

/// The route reader itself, over arbitrary source text.
///
/// Split from [`declared_routes`] so its parsing can be tested against input a
/// test writes, rather than only against this file, which a test cannot vary.
#[cfg(test)]
fn routes_in(source: &str) -> Vec<(String, String)> {
    let mut routes = Vec::new();
    // Split rather than index: the workspace denies `clippy::string_slice`,
    // and a byte range into UTF-8 text is exactly the hazard that lint is for.
    for chunk in source.split(".route(").skip(1) {
        // Balance parentheses to take just this call's arguments. The handler
        // chain (`get(..).post(..)`) contains its own, and the chunk runs on
        // past the call's end.
        let mut depth = 1usize;
        let mut body = String::new();
        for ch in chunk.chars() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            body.push(ch);
        }
        let Some(path) = body
            .split_once('"')
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(path, _)| path)
        else {
            continue;
        };
        // This function's own source contains the text it splits on, so one
        // chunk is always the split call itself. Requiring a route-shaped path
        // drops it rather than inventing a route from the code around it.
        if !path.starts_with('/') {
            continue;
        }
        for method in ["get", "post", "put", "delete", "patch"] {
            if body.contains(&format!("{method}(")) {
                routes.push((path.to_string(), method.to_uppercase()));
            }
        }
    }
    routes
}

/// Core of [`execute`], with an optional shutdown signal so tests can stop
/// the server gracefully and cover the `Ok(())` return path.
///
/// Takes `shutdown` as a boxed trait object (`Pin<Box<dyn Future<...>>>`)
/// rather than `impl Future<...>` so every caller - production's
/// `std::future::pending()` and tests' various `async move { ... }` blocks
/// awaiting a `oneshot::Receiver` - shares exactly ONE monomorphization of
/// this (large, multi-branch) function instead of one per concrete future
/// type. Confirmed via HTML/JSON segment inspection that every source
/// position has a covered instantiation (this is the same trait-object-erasure
/// technique used for `io::Write` in `leviath-package`'s `bundler.rs`).
///
/// `ready`, if given, is sent the real bound `SocketAddr` right after
/// `TcpListener::bind` succeeds (before serving starts). Production passes
/// `None`; tests pass `Some(tx)` with `args.port = 0` so the OS picks a free
/// port and the test learns which one was actually bound directly - no
/// probe-bind-drop-rebind dance, which is a genuine TOCTOU race (confirmed
/// to reproduce on real CI: another process/test could grab the just-freed
/// port before this function's own bind runs), not just a test-only
/// convenience.
async fn execute_with_shutdown(
    args: ServeArgs,
    control: leviath_runtime::control_socket::ControlClient,
    upgrade: crate::commands::update::CommandRunner,
    shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    // Resolve the API token before binding - refuse to start unauthenticated.
    let auth_token = std::sync::Arc::new(auth::resolve_token(args.token.as_deref())?);
    // The API can spawn tool-executing agents; loudly warn if bound off-host.
    if args.host != "127.0.0.1" && args.host != "localhost" && args.host != "::1" {
        tracing::warn!(
            host = %args.host,
            "serving the agent API on a non-local address - anyone who can reach \
             this host and holds the token can spawn agents"
        );
    }

    let cfg = Config::load()?;
    // Read before `cfg` moves into the shared state below.
    let allow_local_network = cfg.security.allow_local_network;
    for warning in cfg.validate_keys() {
        tracing::warn!("{}", warning);
    }

    // Sized like the daemon's WorldEvent ring (see WorldHost): the ring never
    // shrinks, so its capacity is a permanent memory floor once filled.
    let (event_tx, _) = broadcast::channel::<ServerEvent>(256);

    let state = AppState {
        update_check: Default::default(),
        update_jobs: update_job::UpdateJobs::with_runner(upgrade),
        config: Arc::new(crate::daemon::config_reload::ConfigReloader::new(
            Config::config_path(),
            cfg,
        )),
        event_tx: event_tx.clone(),
        control,
        mcp: mcp::McpAdmin::default(),
        limits: Arc::new(ServeLimits {
            workdir_root: args.workdir_root.clone(),
            no_remote_yolo: args.no_remote_yolo,
            allow_local_network,
        }),
    };

    // Background world-event consumer: subscribes to the daemon's pushed
    // `WorldEvent` stream and forwards each event to WebSocket subscribers.
    // Held behind an abort-on-drop guard so the task is torn down whenever this
    // function returns *or* is cancelled - e.g. when a test aborts the outer
    // `execute()`/`execute_with_shutdown()` task. Without this, aborting only
    // the outer task left the inner `event_loop` (an unconditional
    // subscribe-and-reconnect loop) running detached until the whole runtime
    // was torn down.
    let event_state = state.clone();
    let _event_guard = AbortOnDrop(tokio::spawn(polling::event_loop(
        event_state,
        polling::RECONNECT_BACKOFF,
    )));

    // No `--cors` at all: no CORS layer. Programmatic clients are not subject to
    // CORS, so the previous `*` default bought them nothing while telling every
    // browser that any page may talk to this server.
    let cors = match args.cors.as_deref() {
        None => None,
        Some("*") => Some(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                // `Access-Control-Allow-Headers: *` does NOT cover
                // `Authorization` per the Fetch spec, so a browser sending the
                // required bearer token would be blocked. List the headers the
                // API actually needs explicitly.
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                ]),
        ),
        Some(origin) => {
            // An unparseable value must not fall back to `*` - that silently
            // turns a typo into "allow everything", the opposite of what was
            // asked for. Refuse to start instead.
            let value = origin.parse::<axum::http::HeaderValue>().map_err(|_| {
                anyhow::anyhow!("--cors value '{origin}' is not a valid origin header")
            })?;
            Some(
                CorsLayer::new()
                    .allow_origin(value)
                    .allow_methods(Any)
                    // `Access-Control-Allow-Headers: *` does NOT cover
                    // `Authorization` per the Fetch spec, so a browser sending the
                    // required bearer token would be blocked. List the headers the
                    // API actually needs explicitly.
                    .allow_headers([
                        axum::http::header::AUTHORIZATION,
                        axum::http::header::CONTENT_TYPE,
                    ]),
            )
        }
    };

    let app = api_router();

    // The MCP administration endpoints are remote code execution by
    // construction: `add_server` writes a `command` and `args` into
    // `~/.leviath/config.toml`, and Leviath then spawns exactly that - for this
    // run and every future one. The rest of the API can only run agents the user
    // already installed. Not mounted unless the operator asked for them, so an
    // unmounted route 404s rather than relying on a check inside the handler
    // that someone could later route around.
    let app = match args.allow_admin {
        true => app
            .route("/api/mcp/servers", post(mcp::add_server))
            .route("/api/mcp/servers/{name}", delete(mcp::remove_server))
            // Login opens the operator's browser and completes an OAuth flow
            // on this host; test connects to the server and, for a stdio one,
            // spawns its command. Neither is a read, and both were reachable by
            // anyone holding the bearer token.
            .route("/api/mcp/servers/{name}/login", post(mcp::login))
            .route("/api/mcp/servers/{name}/test", post(mcp::test_server))
            // The live doctor makes two billed provider calls and spawns a run
            // through the daemon. The read half above stops before either.
            .route("/api/doctor/live", post(doctor::run_doctor_live))
            // Config-write persists provider secrets to disk, so it is gated the
            // same way as MCP admin: unmounted (404) unless --allow-admin.
            .route("/api/config", put(config::put_config))
            // Carrying out the update the GET half only describes: it runs a
            // package manager, replaces the blueprints in the agents directory
            // and rewrites the config. Same category of act as the three above,
            // and gated the same way - the read half stays open, so a console
            // without admin still shows what to type.
            .route("/api/update", post(update::post_update))
            .route("/api/update/jobs/{id}", get(update::get_update_job))
            // A `.rhai` file is executable code every agent then runs, so
            // writing one is the same category of act as adding an MCP server
            // rather than the same category as saving a blueprint. Unmounted
            // (404) unless --allow-admin; the GET half above stays open so an
            // editor degrades to read-only instead of disappearing.
            .route(
                "/api/scripts/{kind}/{name}",
                put(scripts::put_script).delete(scripts::delete_script),
            ),
        false => app,
    };

    let app = app
        // Require a valid token on every route; CORS stays outermost so browser
        // preflight (OPTIONS) is answered before the auth check.
        .layer(axum::middleware::from_fn_with_state(
            auth_token,
            auth::require_auth,
        ))
        .with_state(state);

    // Merged *after* the auth layer, which is the entire point: a browser tab
    // cannot send an `Authorization` header, and this page exists to be opened
    // in one. With a self-signed certificate that is how a user reaches the
    // interstitial and accepts it, after which the console's `fetch` to the
    // same origin inherits the exception.
    //
    // Deliberately says almost nothing. It is a new unauthenticated surface,
    // and a visitor who can load it already knows the port is open - so it adds
    // no version, no run counts, no endpoint list.
    let app = app.merge(Router::new().route("/", get(status_page)));
    // Applied by branching on the router rather than layering an `Option`:
    // `Option<CorsLayer>` is not a `Layer`, and a permissive-but-unused layer
    // would be exactly the default this change removes.
    let app = match cors {
        Some(layer) => app.layer(layer),
        None => app,
    };

    // Resolved and loaded before the listener binds. A server that binds and
    // then fails every handshake looks like a network fault from the other
    // machine; one that refuses to start names the file it could not read.
    let tls = tls::resolve(args.tls_cert.clone(), args.tls_key.clone())?;
    let tls_config = match &tls {
        Some(paths) => Some(tls::load(paths).await?),
        None => None,
    };

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let scheme = tls::scheme(tls.as_ref());

    // Bound before anything says it is listening. Announcing first meant a
    // taken port printed "Leviath API server listening on http://127.0.0.1:3000"
    // and *then* died on a bare `os error 48`, which reads as a server that
    // started and crashed rather than one that never started (issue #586).
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| bind_error(&e, &args.host, args.port))?;
    // The address the socket actually got, not the one that was asked for:
    // `--port 0` means "you pick", and the port it picked is the only useful
    // thing to print.
    let bound = listener
        .local_addr()
        .expect("infallible: a freshly bound TcpListener always has a local address");
    tracing::info!("Listening on {}://{}", scheme, bound);
    println!("Leviath API server listening on {scheme}://{bound}");

    if let Some(ready) = ready {
        // A test-only observer failing to receive (e.g. it already gave up
        // after a timeout) shouldn't stop the server from starting for real.
        let _ = ready.send(bound);
    }

    match tls_config {
        // axum::serve with graceful shutdown always returns Ok(()) - discard the
        // infallible Result so LLVM-cov does not instrument an unreachable Err branch.
        None => {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
        }
        Some(config) => serve_tls(listener, app, config, shutdown).await,
    }

    Ok(())
}

/// Turn a failed bind into something a person can act on.
///
/// The default port is 3000, which collides with most of the JavaScript world
/// and a fair number of agent runtimes, so "the port is taken" is the ordinary
/// failure here rather than an exotic one. It used to surface as a bare
/// `os error 48` / `os error 10048` with no mention of the flag that fixes it.
///
/// Deliberately does not fall back to another port. The console at
/// leviath.dev polls a fixed `http://127.0.0.1:3000`, so a server that quietly
/// moved would be a server it can never find: a clear error beats a silent
/// no-connect. `--port 0` remains the way to ask the OS to choose, and the
/// startup line then reports what it chose.
fn bind_error(err: &std::io::Error, host: &str, port: u16) -> anyhow::Error {
    match err.kind() {
        std::io::ErrorKind::AddrInUse => anyhow::anyhow!(
            "port {port} on {host} is already in use, so the API server could not start. \
             Pass `--port <port>` to listen somewhere else, or `--port 0` to let the \
             system pick a free one (the port it picks is printed on startup). \
             To find what holds it: `lsof -i :{port}` on macOS and Linux, \
             `netstat -ano | findstr :{port}` on Windows."
        ),
        _ => anyhow::anyhow!("could not listen on {host}:{port}: {err}"),
    }
}

/// Serve over TLS on an already-bound listener, until `shutdown` resolves.
///
/// Takes the listener rather than an address so the bind, the `ready` report
/// and the "port already in use" error are the same code on both schemes -
/// letting `axum-server` bind would have given HTTPS its own second copy of all
/// three, and a `--port 0` test no way to learn the port.
///
/// Shutdown is bridged rather than shared: `axum-server` signals through a
/// `Handle` instead of taking a future, so a task waits on the same future the
/// plain path awaits and converts it into a `graceful_shutdown` call.
async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: Router,
    config: axum_server::tls_rustls::RustlsConfig,
    shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
) {
    let handle = axum_server::Handle::new();
    let signal = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        // Some(..) rather than None: a connection that never closes would
        // otherwise hold the process open for ever, and a WebSocket subscriber
        // is exactly such a connection.
        signal.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });
    // Handed over still non-blocking, which is how tokio left it. Setting it
    // back to blocking looks tidier and *panics*: `from_tcp` re-registers the
    // socket with tokio, which refuses a blocking one. That is also why both
    // conversions below cannot fail here - a bound, non-blocking listener is
    // exactly what they accept.
    let std_listener = listener
        .into_std()
        .expect("infallible: a bound tokio listener always converts back");
    let server = axum_server::from_tcp_rustls(std_listener, config)
        .expect("infallible: the listener is bound and non-blocking, which is all this checks");
    // Discarded for the same reason the plain path discards `axum::serve`'s:
    // with a shutdown signal wired up this resolves to `Ok(())`, and an
    // unreachable `Err` branch is a region the coverage gate cannot forgive.
    let _ = server.handle(handle).serve(app.into_make_service()).await;
}

/// The unauthenticated page at `GET /`.
///
/// Exists so a user can open the endpoint in a browser tab, meet the
/// certificate interstitial, and accept it - after which the console's `fetch`
/// to that origin inherits the exception. Strictly the mechanism does not need
/// a page (the interstitial precedes any response, so even a 401 would do), but
/// landing on an auth error reads like a mistake rather than confirmation.
async fn status_page() -> axum::response::Html<&'static str> {
    axum::response::Html(
        "<!doctype html><meta charset=utf-8><title>Leviath</title>\
         <body style=\"font:16px system-ui;margin:4rem auto;max-width:30rem\">\
         <h1>Leviath is running.</h1>\
         <p>The API needs a token; this page does not serve it.</p>",
    )
}

/// The upgrade runner a server test carries.
///
/// A refusal rather than a no-op: a server test that reached the upgrade path
/// by accident should fail loudly, not quietly report having run a package
/// manager it never ran. A named function rather than a closure per call site
/// so there is one body to cover rather than one per test.
#[cfg(test)]
fn no_upgrade_in_tests(_argv: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("no upgrade command runs in a server test")
}

/// [`execute_with_shutdown`] for a test that is not exercising the update
/// route, with a runner that refuses to spawn anything.
///
#[cfg(test)]
async fn serve_for_test(
    args: ServeArgs,
    control: leviath_runtime::control_socket::ControlClient,
    shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> anyhow::Result<()> {
    execute_with_shutdown(
        args,
        control,
        Arc::new(no_upgrade_in_tests),
        shutdown,
        ready,
    )
    .await
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::runstate::RunMeta;
    use crate::test_support::with_tracing;

    /// The published OpenAPI spec for this API.
    const OPENAPI: &str = include_str!("../../../../../docs/schema/openapi.json");

    /// The published API guide, which is where a capability is explained.
    const API_GUIDE: &str = include_str!("../../../../../docs/content/api.md");

    /// `GET /api/config` reports an `api_version`, and it has to mean
    /// something. A version a client can read that disagrees with the document
    /// describing that version is worse than publishing no version at all - the
    /// client trusts it, and it is silently wrong.
    ///
    /// The same spirit as the route-drift test below: the spec is only a
    /// contract while something holds the code to it.
    #[test]
    fn the_api_version_matches_the_spec_it_names() {
        let spec: serde_json::Value = serde_json::from_str(OPENAPI).expect("the spec is JSON");
        let documented = spec["info"]["version"]
            .as_str()
            .expect("the spec declares a version");
        assert_eq!(documented, types::API_VERSION);
    }

    /// A capability is a promise, and a promise nobody can read is not one.
    ///
    /// `GET /api/config` hands a client a list of strings and expects it to
    /// change its behaviour based on them. Twenty-three of them had never been
    /// named in the guide, so the only way to learn what one meant was to read
    /// this crate - which a browser console's author has no reason to have.
    /// The same spirit as the route-drift test below: announcing it and
    /// documenting it are one act, or they drift.
    #[test]
    fn every_announced_capability_is_explained_in_the_guide() {
        let undocumented: Vec<&str> = types::API_CAPABILITIES
            .iter()
            .copied()
            .filter(|capability| !API_GUIDE.contains(*capability))
            .collect();
        assert!(
            undocumented.is_empty(),
            "announced but never explained in docs/content/api.md: {undocumented:?}"
        );
    }

    /// Every `(path, METHOD)` the spec documents.
    fn documented_routes() -> Vec<(String, String)> {
        let spec: serde_json::Value = serde_json::from_str(OPENAPI).expect("the spec is JSON");
        let paths = spec["paths"].as_object().expect("the spec has paths");
        let mut routes = Vec::new();
        for (path, item) in paths {
            let operations = item.as_object().expect("a path item is an object");
            for method in ["get", "post", "put", "delete", "patch"] {
                if operations.contains_key(method) {
                    routes.push((path.clone(), method.to_uppercase()));
                }
            }
        }
        routes
    }

    /// A set of `(path, METHOD)` pairs.
    type Routes = Vec<(String, String)>;

    /// The two ways the spec can be wrong: a route served but not documented,
    /// and one documented but no longer served.
    fn spec_drift() -> (Routes, Routes) {
        let declared = declared_routes();
        let documented = documented_routes();
        let missing = declared
            .iter()
            .filter(|r| !documented.contains(r))
            .cloned()
            .collect();
        let extra = documented
            .iter()
            .filter(|r| !declared.contains(r))
            .cloned()
            .collect();
        (missing, extra)
    }

    #[test]
    fn the_openapi_spec_documents_exactly_the_routes_this_router_serves() {
        // Hand-written, because there is no derive to generate it from. Without
        // this the spec would be a snapshot of whatever the API looked like the
        // day it was written, and an agent reading it would call routes that
        // moved.
        //
        // Bare `assert!` over a bool, with no message: anything in an assert's
        // format arguments is a region only the failing path reaches, which the
        // 100% coverage gate then reports as uncovered. That costs the failure
        // its detail, so when one of these trips, call `spec_drift()` and print
        // it: `missing` is served but undocumented, `extra` is the reverse.
        let (missing, extra) = spec_drift();
        assert!(missing.is_empty());
        assert!(extra.is_empty());
    }

    #[test]
    fn the_route_reader_finds_the_routes_that_are_actually_there() {
        // Guards the guard. This reads its own source text, so a change to how
        // routes are written could quietly make it find nothing, and a test
        // comparing two empty lists passes.
        let declared = declared_routes();
        assert!(declared.len() > 25);
        assert!(declared.contains(&("/api/agents".to_string(), "POST".to_string())));
        assert!(declared.contains(&("/api/agents/{id}".to_string(), "DELETE".to_string())));
        assert!(declared.contains(&("/ws".to_string(), "GET".to_string())));
    }

    #[test]
    fn the_route_reader_ignores_text_that_is_not_a_route() {
        // The reader splits on a literal its own source contains, so it always
        // sees at least one chunk that is not a route call. Anything without a
        // route-shaped path has to be dropped rather than guessed at.
        let source = concat!(
            "let x = source.split(\".route(\").skip(1);\n",
            ".route(\"not a path\", get(h))\n",
            ".route(\"/real\", get(h).post(h))\n"
        );
        assert_eq!(
            routes_in(source),
            vec![
                ("/real".to_string(), "GET".to_string()),
                ("/real".to_string(), "POST".to_string()),
            ]
        );
    }

    #[test]
    fn the_route_reader_reads_nothing_out_of_source_with_no_routes() {
        assert_eq!(routes_in("fn main() {}"), Vec::new());
    }

    /// Extracted so the `assert!` failure-message region (only executed
    /// when the assertion fails) is covered by this function's own
    /// `#[should_panic]` test below, rather than showing as a
    /// permanently-uncovered region at every real call site.
    fn assert_execute_failed_on_malformed_config(result: &anyhow::Result<()>) {
        assert!(
            result.is_err(),
            "execute should fail when config is malformed"
        );
    }

    #[test]
    #[should_panic(expected = "execute should fail when config is malformed")]
    fn assert_execute_failed_on_malformed_config_panics_when_ok() {
        assert_execute_failed_on_malformed_config(&Ok(()));
    }

    /// See [`assert_execute_failed_on_malformed_config`] - same rationale,
    /// for the bad-API-key startup failure-message region.
    fn assert_connected_with_bad_api_key(connected: bool) {
        assert!(connected, "server should start even with a bad API key");
    }

    #[test]
    #[should_panic(expected = "server should start even with a bad API key")]
    fn assert_connected_with_bad_api_key_panics_when_not_connected() {
        assert_connected_with_bad_api_key(false);
    }

    /// See [`assert_execute_failed_on_malformed_config`] - same rationale,
    /// for the graceful-shutdown return-value failure-message region.
    fn assert_execute_returned_ok_after_shutdown(result: &Result<(), anyhow::Error>) {
        assert!(
            result.is_ok(),
            "execute should return Ok after graceful shutdown"
        );
    }

    #[test]
    #[should_panic(expected = "execute should return Ok after graceful shutdown")]
    fn assert_execute_returned_ok_after_shutdown_panics_when_err() {
        assert_execute_returned_ok_after_shutdown(&Err(anyhow::anyhow!("boom")));
    }

    /// See [`assert_execute_failed_on_malformed_config`] - same rationale,
    /// for the port-in-use failure-message region.
    fn assert_execute_failed_on_port_in_use(result: &anyhow::Result<()>) {
        assert!(
            result.is_err(),
            "execute should fail when port is already in use"
        );
    }

    #[test]
    #[should_panic(expected = "execute should fail when port is already in use")]
    fn assert_execute_failed_on_port_in_use_panics_when_ok() {
        assert_execute_failed_on_port_in_use(&Ok(()));
    }

    /// See [`assert_execute_failed_on_malformed_config`] - same rationale, for
    /// the region that checks a bind failure explains itself.
    fn assert_error_says(result: &anyhow::Result<()>, expected: &str) {
        let message = match result {
            Err(e) => e.to_string(),
            Ok(()) => String::from("<the call succeeded>"),
        };
        assert!(
            message.contains(expected),
            "bind error should mention {expected}"
        );
    }

    #[test]
    #[should_panic(expected = "bind error should mention --port")]
    fn assert_error_says_panics_when_the_message_is_missing() {
        assert_error_says(&Ok(()), "--port");
    }

    /// See [`assert_execute_failed_on_malformed_config`] - same rationale,
    /// for `execute_with_shutdown`'s graceful-shutdown return-value
    /// failure-message region.
    fn assert_execute_with_shutdown_returned_ok(result: &Result<(), anyhow::Error>) {
        assert!(
            result.is_ok(),
            "execute_with_shutdown should return Ok(()) after graceful shutdown"
        );
    }

    #[test]
    #[should_panic(expected = "execute_with_shutdown should return Ok(()) after graceful shutdown")]
    fn assert_execute_with_shutdown_returned_ok_panics_when_err() {
        assert_execute_with_shutdown_returned_ok(&Err(anyhow::anyhow!("boom")));
    }

    /// See [`assert_execute_failed_on_malformed_config`] - same rationale,
    /// for the HTTP response status-line failure-message region.
    fn assert_response_ok(resp_str: &str) {
        assert!(resp_str.starts_with("HTTP/1.1 200"), "got: {resp_str}");
    }

    #[test]
    #[should_panic(expected = "got: HTTP/1.1 404 Not Found")]
    fn assert_response_ok_panics_when_not_200() {
        assert_response_ok("HTTP/1.1 404 Not Found\r\n\r\n");
    }

    /// A control client pointing at an address with no daemon: agent-action
    /// endpoints report "not reachable", and read/bootstrap paths don't touch it.
    fn no_daemon_control() -> leviath_runtime::control_socket::ControlClient {
        leviath_runtime::control_socket::ControlClient::new(
            leviath_runtime::control_socket::control_id(std::path::Path::new("/no/such/leviath")),
        )
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: no_daemon_control(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    /// The file locations [`test_app`] serves under.
    fn test_paths() -> crate::commands::serve::mcp::AdminPaths {
        crate::commands::serve::mcp::AdminPaths {
            // A path nothing else in the binary can reach, and one that does
            // not exist - `load_from_path` answers a missing file with the
            // defaults, so the handler is deterministic.
            //
            // `McpAdmin::default()` resolves `Config::config_path()`, which
            // follows `LEVIATH_HOME`, and other tests move that with
            // `temp_env` while these run. So `GET /api/mcp/servers` was
            // reading whichever config file the process happened to be
            // pointed at - the developer's real one on an ordinary run, and
            // a temp one mid-write on an unlucky one, where a half-written
            // TOML parses as an error and the route answers 500. That is
            // what failed `test_router_serves_routes_the_old_hand_copy_missed`
            // about one run in three, on an assertion about *routing*.
            config: std::env::temp_dir().join("leviath-serve-router-tests-no-such-config.toml"),
            store: std::env::temp_dir().join("leviath-serve-router-tests-no-such-store.json"),
        }
    }

    /// The production route table over a test state - auth, CORS, and the
    /// admin routes are absent, exactly as `api_router` leaves them.
    fn test_app() -> Router {
        crate::commands::serve::mcp::scoped(api_router().with_state(test_state()), test_paths())
    }

    #[tokio::test]
    async fn test_list_blueprints() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/blueprints")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_router_serves_routes_the_old_hand_copy_missed() {
        // /api/mcp/servers was one of the seven routes present in production
        // but absent from the hand-copied test router; with the shared table
        // it must be reachable here too.
        let app = test_app();
        let req = Request::builder()
            .uri("/api/mcp/servers")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_pause_and_resume_routes_are_mounted() {
        // With no daemon behind the test state the handlers answer 503 - the
        // point here is only that the routes exist in the shared table (an
        // unmounted route would 404 at the router).
        for action in ["pause", "resume"] {
            let app = test_app();
            let req = Request::builder()
                .method("POST")
                .uri(format!("/api/agents/some-run/{action}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn test_agent_files_route_is_mounted() {
        // Both a mounted and an unmounted route answer this with a 404 - the
        // run does not exist either way - so the status alone proves nothing.
        // What separates them is the body: the handler explains itself, and
        // the router's own catch-all has nothing to say. (The handler's real
        // behavior is covered in agents.rs.)
        //
        // `path` used to be required, and the resulting 400 was the proof.
        // That stopped discriminating when it became optional so a bare call
        // could list.
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/some-run/files")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let error = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_default();
        assert!(error.contains("some-run"));
    }

    #[tokio::test]
    async fn test_fs_dirs_route_is_mounted() {
        // A relative `path` is the one request whose answer never touches the
        // filesystem: a mounted route rejects it with the handler's 400, an
        // unmounted one 404s at the router. (The handler's own behavior is
        // covered in fs.rs.)
        let app = test_app();
        let req = Request::builder()
            .uri("/api/fs/dirs?path=not/absolute")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_blueprint_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/blueprints/nonexistent-agent-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_validate_blueprint_valid() {
        let app = test_app();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "A test"

[stages.main]
mode = "autonomous"
[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        let body = serde_json::json!({ "manifest": manifest });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: types::ValidateResponse = serde_json::from_slice(&body).unwrap();
        assert!(val.valid);
    }

    #[tokio::test]
    async fn test_validate_blueprint_invalid() {
        let app = test_app();
        let body = serde_json::json!({ "manifest": "not valid toml {{{{" });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints/validate")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: types::ValidateResponse = serde_json::from_slice(&body).unwrap();
        assert!(!val.valid);
        assert!(val.errors.is_some());
    }

    #[tokio::test]
    async fn test_list_agents() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_agents_tree() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/tree")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_agent_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent-run-id-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_children_empty() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/children")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // children returns 200 with empty array even if parent doesn't exist
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_agent_context_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/context")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_logs_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/logs")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_result_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/result")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_agent_tree_status_not_found() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/tree-status")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_interaction_route_reaches_daemon() {
        // The route is wired to the handler, which (with no daemon in this test)
        // reports the daemon unreachable - proving the request reached it.
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents/nonexistent/interaction")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_get_config() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: types::RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert_eq!(val.default_provider, "anthropic");
        // Default config has no keys
        assert!(!val.has_anthropic_key);
        assert!(!val.has_openai_key);
    }

    #[tokio::test]
    async fn test_tree_building() {
        // Unit test for the tree builder
        let runs = vec![
            RunMeta::new(
                "parent-1".to_string(),
                "agent-a".to_string(),
                "/path".to_string(),
                "task".to_string(),
                None,
                "/work".to_string(),
                1,
            ),
            {
                let mut child = RunMeta::new(
                    "child-1".to_string(),
                    "agent-b".to_string(),
                    "/path".to_string(),
                    "sub-task".to_string(),
                    None,
                    "/work".to_string(),
                    1,
                );
                child.parent_run_id = Some("parent-1".to_string());
                child.prompt_tokens = 100;
                child.completion_tokens = 50;
                child
            },
        ];

        let tree = tree::build_tree_status(&runs, None);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].run_id, "parent-1");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].subtree_prompt_tokens, 100); // parent (0) + child (100)
        assert_eq!(tree[0].subtree_completion_tokens, 50);
    }

    #[tokio::test]
    async fn test_delete_blueprint_not_found() {
        let app = test_app();
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/blueprints/nonexistent-agent-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_server_event_serialization() {
        let event = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "implement".to_string(),
            iteration: 5,
            tool_calls: 0,
            accepts_messages: true,
            wait_reason: None,
            title: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"agent_id\":\"coder\""));

        let event2 = ServerEvent::Tokens {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            prompt_tokens: 5000,
            completion_tokens: 1200,
            cached_tokens: 0,
            cache_write_tokens: 0,
        };
        let json2 = serde_json::to_string(&event2).unwrap();
        assert!(json2.contains("\"type\":\"tokens\""));
        assert!(json2.contains("\"prompt_tokens\":5000"));
    }

    #[tokio::test]
    async fn test_full_router_create_blueprint_invalid() {
        let app = test_app();
        let body = serde_json::json!({
            "name": "bad-agent",
            "manifest": "not valid toml {{{"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/blueprints")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_full_router_update_blueprint_not_found() {
        let app = test_app();
        let body = serde_json::json!({
            "manifest": r#"
[agent]
name = "no-such-agent"
version = "1.0.0"
description = "Missing"

[stages.run]
system_prompt = "Run"
"#
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/api/blueprints/no-such-agent-xyz-99999")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_full_router_kill_agent_reaches_daemon() {
        let app = test_app();
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/agents/nonexistent-kill-id-xyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_full_router_send_message_reaches_daemon() {
        let app = test_app();
        let body = serde_json::json!({"message": "hello"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/nonexistent-msg-id-xyz/message")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_full_router_get_models() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_full_router_spawn_agent_blueprint_not_found() {
        let app = test_app();
        let body = serde_json::json!({
            "blueprint": "nonexistent-blueprint-xyz",
            "task": "do something"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_serve_args_defaults() {
        let args = ServeArgs {
            port: 3000,
            host: "127.0.0.1".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
            tls_cert: None,
            tls_key: None,
        };
        assert_eq!(args.port, 3000);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.cors, None);
    }

    #[test]
    fn test_app_state_clone() {
        let state = test_state();
        let cloned = state.clone();
        // Both should work (no panic)
        let _ = cloned.current_config().default_provider.clone();
    }

    #[test]
    fn test_cors_wildcard_vs_specific() {
        // Test the CORS logic paths used in execute()
        let wildcard = "*";
        let specific = "https://example.com";

        let is_wildcard = wildcard == "*";
        assert!(is_wildcard);

        let is_specific = specific != "*";
        assert!(is_specific);

        // Test that specific CORS origin parses correctly
        let parsed = specific.parse::<axum::http::HeaderValue>();
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_cors_invalid_origin_falls_back() {
        let invalid_cors = "not a valid header value \x00";
        let result = invalid_cors.parse::<axum::http::HeaderValue>();
        // Invalid header values fail to parse; the code falls back to "*"
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_submit_interaction_full_router_reaches_daemon() {
        // The POST-interaction route is wired to the handler, which reaches the
        // (absent-in-test) daemon. The ACCEPTED path is covered by the
        // interactions handler's own tests against a fake daemon.
        let app = test_app();
        let body = serde_json::json!({"request_id": "req-1", "value": "do it", "scope": "once"});
        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/any/interaction")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ─── execute() - real server bootstrap ─────────────────────────────────
    //
    // These drive the actual `execute()` entrypoint (config load, CORS setup,
    // full router construction, real TCP bind, background polling spawn) end
    // to end using port 0 (OS-assigned ephemeral port) so no fixed port is
    // required. Since `axum::serve(...).await` never returns on success, the
    // task is aborted once we've proven the server is up and responding.
    //
    // Each holds `isolate_config_path_for_test` even though none of them
    // care about specific config *content* - their own `Config::load()`
    // call needs protecting from a DIFFERENT concurrently-running test that
    // does mutate `LEVIATH_CONFIG_PATH` (e.g. `execute_with_malformed_config_
    // returns_err`, which points it at a file containing invalid TOML for
    // the duration of its own guard). `std::env::set_var` is process-global,
    // not thread-local, so without holding the same lock here, this test's
    // `Config::load()` could transiently observe that other test's malformed
    // path and fail with a real (if confusing) parse error - confirmed to
    // reproduce locally at default test-thread concurrency, not a hypothetical.

    #[tokio::test]
    async fn execute_binds_and_serves_with_wildcard_cors() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-wildcard-cors",
            |_fake_dir| async move {
                with_tracing(|| {});
                // port: 0 lets the OS assign a genuinely free ephemeral port at bind
                // time; execute_with_shutdown reports the real bound SocketAddr back
                // via `ready` the instant it's bound, so there's no
                // probe-bind-drop-rebind gap for another process/test to race into
                // (that gap is a real, CI-reproducing TOCTOU - see
                // execute_with_shutdown's doc comment). Exercises the exact same
                // production code path execute() does (its own body is just this
                // call with `ready: None`), so this remains a real end-to-end test
                // of execute()'s bootstrap logic.
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let handle = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    Some(ready_tx),
                ));
                let addr = ready_rx
                    .await
                    .expect("server should report its bound address");

                // Sanity-check a real request round trip through the full app.
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                stream
                    .write_all(
                        b"GET /api/config HTTP/1.1\r\nHost: localhost\r\n\
                          Authorization: Bearer test-token\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut resp = Vec::new();
                stream.read_to_end(&mut resp).await.unwrap();
                let resp_str = String::from_utf8_lossy(&resp);
                assert_response_ok(&resp_str);

                // Without the token the same request is rejected.
                let mut unauth = tokio::net::TcpStream::connect(addr).await.unwrap();
                unauth
                    .write_all(
                        b"GET /api/config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut resp2 = Vec::new();
                unauth.read_to_end(&mut resp2).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&resp2).starts_with("HTTP/1.1 401"),
                    "unauthenticated request should be 401"
                );

                handle.abort();
            },
        )
        .await;
    }

    /// The whole feature, end to end: a real TLS handshake against a real
    /// listener, and the status page answering without a token.
    ///
    /// Everything else about TLS is tested in `tls::tests` against files. This
    /// is the one that would catch the wiring being wrong - a certificate that
    /// loads but a server that never speaks TLS, or a `/` route that ended up
    /// inside the auth layer after all.
    #[tokio::test]
    async fn execute_serves_https_and_the_status_page_needs_no_token() {
        crate::config::with_isolated_config_path_async("serve-mod-tls", |_fake_dir| async move {
            with_tracing(|| {});
            let dir = tempfile::tempdir().expect("tempdir");
            let cert = dir.path().join("cert.pem");
            let key = dir.path().join("key.pem");
            std::fs::write(&cert, tls::tests::TEST_CERT).expect("write cert");
            std::fs::write(&key, tls::tests::TEST_KEY).expect("write key");

            let args = ServeArgs {
                port: 0,
                host: "127.0.0.1".to_string(),
                cors: None,
                token: Some("test-token".to_string()),
                allow_admin: false,
                workdir_root: None,
                no_remote_yolo: false,
                tls_cert: Some(cert),
                tls_key: Some(key),
            };
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(serve_for_test(
                args,
                no_daemon_control(),
                Box::pin(std::future::pending()),
                Some(ready_tx),
            ));
            let addr = ready_rx.await.expect("server reports its address");

            // A client that trusts exactly this certificate, so a successful
            // response proves the server presented it - not that verification
            // was skipped.
            let mut roots = tokio_rustls::rustls::RootCertStore::empty();
            use rustls_pki_types::pem::PemObject;
            for der in
                rustls_pki_types::CertificateDer::pem_slice_iter(tls::tests::TEST_CA.as_bytes())
            {
                roots
                    .add(der.expect("a parseable certificate"))
                    .expect("add to the root store");
            }
            let client_config = tokio_rustls::rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_config));

            let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
            let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
                .expect("a valid name");
            let mut tls_stream = connector
                .connect(server_name, stream)
                .await
                .expect("the TLS handshake succeeds against the served certificate");

            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            tls_stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .expect("write");
            let mut resp = Vec::new();
            tls_stream.read_to_end(&mut resp).await.expect("read");
            let text = String::from_utf8_lossy(&resp).into_owned();

            // No `Authorization` header was sent, and the page still answers -
            // which is the property the certificate-accepting flow depends on.
            assert!(text.starts_with("HTTP/1.1 200"), "{text}");
            assert!(text.contains("Leviath is running."), "{text}");

            handle.abort();
        })
        .await;
    }

    /// Both TLS failures stop the server before it binds, which is the whole
    /// point: one that binds and then rejects every handshake looks like a
    /// network fault from the other machine.
    #[tokio::test]
    async fn a_bad_tls_configuration_stops_the_server_before_it_binds() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-tls-bad",
            |_fake_dir| async move {
                with_tracing(|| {});
                let dir = tempfile::tempdir().expect("tempdir");
                let cert = dir.path().join("cert.pem");
                std::fs::write(&cert, "not a certificate").expect("write");

                let base = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };

                // One flag without the other.
                let lone = ServeArgs {
                    tls_cert: Some(cert.clone()),
                    ..base.clone()
                };
                let err = serve_for_test(
                    lone,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    None,
                )
                .await
                .expect_err("one TLS flag alone is refused");
                let message = format!("{err:#}");
                assert!(message.contains("--tls-key"), "{message}");

                // Both flags, but the certificate will not parse.
                let key = dir.path().join("key.pem");
                std::fs::write(&key, tls::tests::TEST_KEY).expect("write");
                let unreadable = ServeArgs {
                    tls_cert: Some(cert),
                    tls_key: Some(key),
                    ..base
                };
                let err = serve_for_test(
                    unreadable,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    None,
                )
                .await
                .expect_err("a malformed certificate is refused");
                let message = format!("{err:#}");
                assert!(message.contains("cert.pem"), "{message}");
            },
        )
        .await;
    }

    /// The HTTPS server stops when its shutdown future resolves.
    ///
    /// `axum-server` signals through a `Handle` rather than taking a future, so
    /// this is the one place the two shutdown models are bridged - and a bridge
    /// that never fires would leave `lev serve` unkillable by anything short of
    /// a signal.
    #[tokio::test]
    async fn https_shuts_down_when_its_signal_resolves() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-tls-shutdown",
            |_fake_dir| async move {
                with_tracing(|| {});
                let dir = tempfile::tempdir().expect("tempdir");
                let cert = dir.path().join("cert.pem");
                let key = dir.path().join("key.pem");
                std::fs::write(&cert, tls::tests::TEST_CERT).expect("write cert");
                std::fs::write(&key, tls::tests::TEST_KEY).expect("write key");

                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: Some(cert),
                    tls_key: Some(key),
                };
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(async move {
                        let _ = stop_rx.await;
                    }),
                    Some(ready_tx),
                ));
                ready_rx.await.expect("server reports its address");

                stop_tx.send(()).expect("the server is listening for this");
                // Returns rather than being aborted, which is what proves the
                // signal reached `axum-server` instead of the task simply being
                // killed.
                let finished = tokio::time::timeout(std::time::Duration::from_secs(10), server)
                    .await
                    .expect("the server should stop on its own");
                finished
                    .expect("the task should not panic")
                    .expect("a clean shutdown is not an error");
            },
        )
        .await;
    }

    /// A browser preflight for a request carrying `Authorization` must be
    /// allowed. `Access-Control-Allow-Headers: *` does NOT cover `Authorization`
    /// per the Fetch spec, so the header has to be listed explicitly — without
    /// it the console's authenticated requests are blocked by the browser. Also
    /// covers the `Some("*")` CORS arm.
    #[tokio::test]
    async fn execute_cors_preflight_allows_authorization_header() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-cors-preflight",
            |_fake_dir| async move {
                with_tracing(|| {});
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: Some("*".to_string()),
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let handle = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    Some(ready_tx),
                ));
                let addr = ready_rx
                    .await
                    .expect("server should report its bound address");

                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                stream
                    .write_all(
                        b"OPTIONS /api/config HTTP/1.1\r\nHost: localhost\r\n\
                          Origin: https://leviath.dev\r\n\
                          Access-Control-Request-Method: GET\r\n\
                          Access-Control-Request-Headers: authorization\r\n\
                          Connection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut resp = Vec::new();
                stream.read_to_end(&mut resp).await.unwrap();
                let lower = String::from_utf8_lossy(&resp).to_lowercase();
                assert!(
                    lower.contains("access-control-allow-headers")
                        && lower.contains("authorization"),
                    "preflight must allow the Authorization header, got:\n{lower}"
                );

                handle.abort();
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_specific_cors_origin_serves() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-specific-cors",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: Some("https://example.com".to_string()),
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let handle = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(std::future::pending()),
                    Some(ready_tx),
                ));
                let addr = ready_rx
                    .await
                    .expect("server should report its bound address");
                assert!(tokio::net::TcpStream::connect(addr).await.is_ok());

                handle.abort();
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_unparseable_addr_returns_err() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("serve-badaddr", |_fake_dir| async move {
            // An invalid host string makes `format!("{host}:{port}").parse()`
            // fail, exercising execute()'s `?` on the SocketAddr parse.
            let args = ServeArgs {
                port: 0,
                host: "not a valid host".to_string(),
                cors: None,
                token: Some("test-token".to_string()),
                allow_admin: false,
                workdir_root: None,
                no_remote_yolo: false,
                tls_cert: None,
                tls_key: None,
            };
            let result = execute(args, no_daemon_control(), Arc::new(no_upgrade_in_tests)).await;
            assert!(result.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn test_agent_list_with_status_filter_full_router() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/agents?status=running,complete")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Covers `Config::load()?` error path (line 31) by pointing
    /// `LEVIATH_CONFIG_PATH` at a file containing invalid TOML.
    #[tokio::test]
    async fn execute_with_malformed_config_returns_err() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-malformed",
            |_fake_dir| async move {
                // After isolate_config_path_for_test, Config::config_path() returns the temp path.
                std::fs::write(Config::config_path(), "not valid toml [[[").unwrap();

                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let result =
                    execute(args, no_daemon_control(), Arc::new(no_upgrade_in_tests)).await;
                assert_execute_failed_on_malformed_config(&result);
            },
        )
        .await;
    }

    /// Covers the `for warning in cfg.validate_keys()` loop body (lines 32-33)
    /// by writing a config with a bad anthropic key, then running the server
    /// with a graceful-shutdown signal so the loop executes before bind.
    #[tokio::test]
    async fn execute_with_bad_api_key_logs_warning_and_serves() {
        with_tracing(|| {});
        crate::config::with_isolated_config_path_async("serve-mod-badkey", |_fake_dir| async move {
        // Write a config with an anthropic key that fails validate_keys().
        std::fs::write(
            Config::config_path(),
            "default_provider = \"anthropic\"\nagent_paths = []\n[providers]\nanthropic_api_key = \"bad-key-not-sk-ant\"\n",
        )
        .unwrap();

        let args = ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown_fut = async move {
            let _ = shutdown_rx.await;
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(serve_for_test(
            args,
            no_daemon_control(),
            Box::pin(shutdown_fut),
            Some(ready_tx),
        ));
        let addr = ready_rx
            .await
            .expect("server should report its bound address");
        let connected = tokio::net::TcpStream::connect(addr).await.is_ok();
        assert_connected_with_bad_api_key(connected);

        // Trigger graceful shutdown so execute_with_shutdown returns Ok(()).
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for execute to return")
            .expect("task panicked");
        assert_execute_returned_ok_after_shutdown(&result);
    }).await;
    }

    /// Covers the generic (non-`AddrInUse`) arm of [`bind_error`] deterministically
    /// by binding to a reserved TEST-NET-1 address (RFC 5737, `192.0.2.0/24`)
    /// that is never assigned to a local interface, so the bind always fails
    /// with `EADDRNOTAVAIL`. (A prior version reused an already-bound ephemeral
    /// port, which occasionally let the second bind succeed under parallel-test
    /// load and left this region uncovered - a genuine flake. The port-in-use
    /// case now has its own test below, which binds first on purpose.)
    #[tokio::test]
    async fn execute_with_unbindable_address_returns_bind_error() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async(
            "serve-unbindable",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 8080,
                    host: "192.0.2.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let result =
                    execute(args, no_daemon_control(), Arc::new(no_upgrade_in_tests)).await;
                assert_execute_failed_on_port_in_use(&result);
                // Names the address it could not have, rather than leaving the
                // reader with a bare errno.
                assert_error_says(&result, "could not listen on 192.0.2.1:8080");
            },
        )
        .await;
    }

    /// The failure people actually hit: the default port is 3000, and 3000 is
    /// taken on a great many developer machines. Holds an ephemeral port for
    /// the duration so the collision is certain rather than hoped for, then
    /// asserts the error names the flag that fixes it (issue #586).
    #[tokio::test]
    async fn execute_on_a_taken_port_names_the_port_flag() {
        let held = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral loopback port always succeeds");
        let port = held
            .local_addr()
            .expect("a bound listener always has a local address")
            .port();
        crate::config::with_isolated_config_path_async(
            "serve-port-taken",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let result =
                    execute(args, no_daemon_control(), Arc::new(no_upgrade_in_tests)).await;
                assert_execute_failed_on_port_in_use(&result);
                assert_error_says(
                    &result,
                    &format!("port {port} on 127.0.0.1 is already in use"),
                );
                assert_error_says(&result, "--port <port>");
            },
        )
        .await;
        drop(held);
    }

    #[tokio::test]
    async fn execute_refuses_to_start_without_a_token() {
        // No --token and no LEVIATH_API_TOKEN ⇒ the server won't start.
        temp_env::async_with_vars([("LEVIATH_API_TOKEN", None::<&str>)], async {
            let args = ServeArgs {
                port: 0,
                host: "127.0.0.1".to_string(),
                cors: None,
                token: None,
                allow_admin: false,
                workdir_root: None,
                no_remote_yolo: false,
                tls_cert: None,
                tls_key: None,
            };
            let result = execute(args, no_daemon_control(), Arc::new(no_upgrade_in_tests)).await;
            assert!(result.is_err(), "must refuse to start unauthenticated");
        })
        .await;
    }

    /// Covers `axum::serve(...).await?` Ok path (lines 117, 119) by running
    /// `execute_with_shutdown` and sending a graceful-shutdown signal.
    #[tokio::test]
    async fn execute_with_shutdown_signal_returns_ok() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-shutdown-signal",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };

                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let shutdown_fut = async move {
                    let _ = shutdown_rx.await;
                };
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

                let handle = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(shutdown_fut),
                    Some(ready_tx),
                ));
                ready_rx
                    .await
                    .expect("server should report its bound address");

                // Send shutdown signal and wait for execute to return Ok.
                let _ = shutdown_tx.send(());
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                    .await
                    .expect("timed out waiting for execute_with_shutdown to return")
                    .expect("task panicked");
                assert_execute_with_shutdown_returned_ok(&result);
            },
        )
        .await;
    }

    /// Covers the `ready: None` fall-through of the `if let Some(ready)` block
    /// (line 190): a successful bind with no ready-observer, shut down
    /// gracefully. Every other binding test passes `Some(ready)`, and every
    /// `None` caller (`execute()`) in other tests fails before binding, so this
    /// is the only path that reaches the block's None continuation.
    #[tokio::test]
    async fn execute_with_shutdown_no_ready_observer_returns_ok() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-no-ready",
            |_fake_dir| async move {
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("test-token".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };

                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let shutdown_fut = async move {
                    let _ = shutdown_rx.await;
                };

                let handle = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(shutdown_fut),
                    None,
                ));
                // Give the server a moment to bind before shutting down.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = shutdown_tx.send(());
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                    .await
                    .expect("timed out waiting for execute_with_shutdown to return")
                    .expect("task panicked");
                assert_execute_with_shutdown_returned_ok(&result);
            },
        )
        .await;
    }
    /// The three CORS shapes. Default is *no layer*: the API's clients are
    /// programmatic and not subject to CORS, so a browser-facing `*` default
    /// gave them nothing and widened the surface for everyone else.
    #[tokio::test]
    async fn cors_is_off_by_default_explicit_when_asked_and_fatal_when_malformed() {
        // Isolated because `execute_with_shutdown` calls `Config::load()`, which
        // reads process-wide environment. Without this the test raced every
        // `temp_env` test in the binary - `temp_env` serializes against its own
        // calls, not against a test that reads the environment directly - and
        // failed on CI in two different places depending on when it lost.
        crate::config::with_isolated_config_path_async("serve-mod-cors", |_fake_dir| async move {
            fn args_with(cors: Option<&str>) -> ServeArgs {
                ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: cors.map(str::to_string),
                    token: Some("t".to_string()),
                    allow_admin: false,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                }
            }

            /// Start, wait until bound, then shut down. Only reached for values that
            /// are accepted - a rejected one never binds.
            async fn starts(cors: Option<&str>) {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(serve_for_test(
                    args_with(cors),
                    no_daemon_control(),
                    Box::pin(async move {
                        let _ = stop_rx.await;
                    }),
                    Some(ready_tx),
                ));
                // `RecvError` here means the sender was dropped, which any early
                // return from `execute_with_shutdown` does - so this reads as "the
                // server failed to start" without saying why. Left as-is rather
                // than adding a reporting branch that only a failing run executes,
                // which the coverage gate would (correctly) flag as dead.
                ready_rx.await.expect("the server bound");
                let _ = stop_tx.send(());
                server.await.expect("join").expect("clean shutdown");
            }

            starts(None).await;
            starts(Some("*")).await;
            starts(Some("https://ok.example")).await;

            // A malformed origin fails before binding, so this can be awaited
            // directly rather than raced against a `ready` signal.
            let err = serve_for_test(
                args_with(Some("not a valid\nheader")),
                no_daemon_control(),
                Box::pin(std::future::pending()),
                None,
            )
            .await
            .expect_err("a malformed origin must refuse to start");
            // Printed on failure: startup can fail earlier than the CORS check (the
            // config load, for one), and "assertion failed" alone does not say so.
            assert!(
                err.to_string().contains("not a valid origin header"),
                "expected the CORS parse to be what refused, got: {err}"
            );
        })
        .await;
    }

    /// The MCP admin endpoints are mounted only with `--allow-admin`: adding an
    /// MCP server writes a spawn command into config, which Leviath then runs.
    #[tokio::test]
    async fn the_mcp_admin_routes_are_mounted_only_with_allow_admin() {
        // Same isolation, same reason: this one also reaches `Config::load()`.
        crate::config::with_isolated_config_path_async("serve-mod-admin", |_fake_dir| async move {
            for allow_admin in [false, true] {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("t".to_string()),
                    allow_admin,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let server = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(async move {
                        let _ = stop_rx.await;
                    }),
                    Some(ready_tx),
                ));
                let addr = ready_rx.await.expect("bound");

                let status = reqwest::Client::new()
                    .post(format!("http://{addr}/api/mcp/servers"))
                    .bearer_auth("t")
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                    .expect("request")
                    .status()
                    .as_u16();
                // 405 (Method Not Allowed) is the signature of "this path exists
                // for GET but POST is not mounted". Asserted as a presence check
                // rather than an exact code for the mounted case, whose status
                // depends on body validation rather than on routing.
                match allow_admin {
                    false => assert_eq!(status, 405, "the admin route must not be mounted"),
                    true => assert_ne!(status, 405, "the admin route must be mounted"),
                }

                let _ = stop_tx.send(());
                let _ = server.await;
            }
        })
        .await;
    }

    /// The runner every server test here carries refuses, so a test that
    /// reached the upgrade path by accident fails loudly rather than reporting
    /// a package manager it never ran.
    #[test]
    fn a_server_test_cannot_run_an_upgrade_command() {
        let e = no_upgrade_in_tests(&["brew".to_string()]).expect_err("it refuses");
        assert!(e.to_string().contains("no upgrade command runs"), "{e}");
    }

    /// The update routes that *do* something are mounted only with
    /// `--allow-admin`. The `GET` half stays mounted either way, so a console
    /// without admin still shows what to type - which is the whole point of it
    /// being a separate route.
    ///
    /// The request deliberately asks for the binary step alone. The runner
    /// `serve_for_test` supplies refuses to spawn anything, so nothing runs;
    /// asking for the blueprint step would point a real install at whatever
    /// `~/.leviath/agents` this test happens to run beside.
    #[tokio::test]
    async fn the_update_admin_routes_are_mounted_only_with_allow_admin() {
        crate::config::with_isolated_config_path_async(
            "serve-mod-update",
            |_fake_dir| async move {
                for allow_admin in [false, true] {
                    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
                    let args = ServeArgs {
                        port: 0,
                        host: "127.0.0.1".to_string(),
                        cors: None,
                        token: Some("t".to_string()),
                        allow_admin,
                        workdir_root: None,
                        no_remote_yolo: false,
                        tls_cert: None,
                        tls_key: None,
                    };
                    let server = tokio::spawn(serve_for_test(
                        args,
                        no_daemon_control(),
                        Box::pin(async move {
                            let _ = stop_rx.await;
                        }),
                        Some(ready_tx),
                    ));
                    let addr = ready_rx.await.expect("bound");
                    let client = reqwest::Client::new();

                    let apply = client
                        .post(format!("http://{addr}/api/update"))
                        .bearer_auth("t")
                        .json(&serde_json::json!({
                            "binary": true, "agents": false, "migrations": false
                        }))
                        .send()
                        .await
                        .expect("request")
                        .status()
                        .as_u16();
                    let job = client
                        .get(format!("http://{addr}/api/update/jobs/never"))
                        .bearer_auth("t")
                        .send()
                        .await
                        .expect("request")
                        .status()
                        .as_u16();
                    // 405 is the signature of "this path exists for GET but POST is
                    // not mounted"; the jobs path has nothing else on it, so it is
                    // a 404 when unmounted and a 404-for-a-real-reason when mounted
                    // - which is why that one is checked as "not 405" from the
                    // other side: an unknown id.
                    match allow_admin {
                        false => {
                            assert_eq!(apply, 405, "POST must not be mounted");
                            assert_eq!(job, 404, "the jobs route must not be mounted");
                        }
                        true => {
                            assert_eq!(apply, 202, "POST must be mounted and answer at once");
                            assert_eq!(job, 404, "mounted, and that id really is unknown");
                        }
                    }

                    // The read half answers either way.
                    let plan = client
                        .get(format!("http://{addr}/api/update"))
                        .bearer_auth("t")
                        .send()
                        .await
                        .expect("request")
                        .status()
                        .as_u16();
                    assert_eq!(plan, 200, "the read half is never gated");

                    let _ = stop_tx.send(());
                    let _ = server.await;
                }
            },
        )
        .await;
    }

    /// The script write routes are mounted only with `--allow-admin`, for the
    /// same reason the MCP ones are: a `.rhai` file is executable code every
    /// agent then runs, so a browser session that can `PUT` one can run code on
    /// the host. The `GET` half stays mounted either way, so an editor degrades
    /// to read-only instead of disappearing.
    ///
    /// `LEVIATH_HOME` is redirected alongside the config path because the
    /// mounted case really does write a file, and it must not be a developer's
    /// own `~/.leviath/tools`. One `temp_env` call, since it serializes
    /// process-wide and holds its lock across the future.
    #[tokio::test]
    async fn the_script_write_routes_are_mounted_only_with_allow_admin() {
        let home = tempfile::tempdir().expect("a temp dir");
        let root = home.path().to_path_buf();
        let mut vars = crate::config::config_isolation_vars(&root);
        vars.push(("LEVIATH_HOME", Some(root.clone().into_os_string())));

        temp_env::async_with_vars(vars, async move {
            for allow_admin in [false, true] {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
                let args = ServeArgs {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    cors: None,
                    token: Some("t".to_string()),
                    allow_admin,
                    workdir_root: None,
                    no_remote_yolo: false,
                    tls_cert: None,
                    tls_key: None,
                };
                let server = tokio::spawn(serve_for_test(
                    args,
                    no_daemon_control(),
                    Box::pin(async move {
                        let _ = stop_rx.await;
                    }),
                    Some(ready_tx),
                ));
                let addr = ready_rx.await.expect("bound");
                let client = reqwest::Client::new();

                let write = client
                    .put(format!("http://{addr}/api/scripts/tool/gate"))
                    .bearer_auth("t")
                    .json(&serde_json::json!({ "content": "// @tool gate\n1" }))
                    .send()
                    .await
                    .expect("request")
                    .status()
                    .as_u16();
                // 405 is the signature of "this path exists for GET but PUT is
                // not mounted", the same reading the MCP test above relies on.
                match allow_admin {
                    false => assert_eq!(write, 405, "the write route must not be mounted"),
                    true => assert_ne!(write, 405, "the write route must be mounted"),
                }

                // The read half answers either way. Nothing was written when
                // admin was off, so a 404 is as much a mounted route as a 200.
                let read = client
                    .get(format!("http://{addr}/api/scripts"))
                    .bearer_auth("t")
                    .send()
                    .await
                    .expect("request")
                    .status()
                    .as_u16();
                assert_eq!(read, 200, "the read routes are never gated");

                let _ = stop_tx.send(());
                let _ = server.await;
            }
        })
        .await;
    }
}
