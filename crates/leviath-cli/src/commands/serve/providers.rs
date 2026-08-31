//! `/api/providers`: the providers that sign in with a browser, and the
//! sign-in itself.
//!
//! `PUT /api/config` can already turn Codex on. It cannot sign anybody in, and
//! a provider that is enabled but not signed in is a provider every run fails
//! against - so a console that could only write the flag could get a user
//! exactly as far as broken. That is what these routes are for.
//!
//! ## Why login does not block
//!
//! The MCP login route holds its request open for the whole OAuth flow, up to
//! its five-minute callback timeout. That is fine for a CLI and wrong for a
//! browser UI: the tab has nothing to draw while it waits, no way to show the
//! URL for a machine whose browser did not open, and no way to give up without
//! losing the flow.
//!
//! So `POST .../login` returns as soon as there is an authorize URL - which is
//! immediately, since it exists the moment the loopback listener binds - and
//! the flow carries on behind it. The caller polls `GET /api/providers`, which
//! reports `waiting`, then `signed_in` or the failure. One extra request buys
//! a UI that can render the whole thing.
//!
//! ## Where the browser has to be
//!
//! On the machine running `lev serve`. The redirect goes to `localhost:1455`
//! there and nowhere else, because that is what the public client id is
//! registered against. A console driving a remote daemon can still start the
//! flow and show the URL, but somebody has to open it on the daemon's host.
//! `authorize_url` is in the response for exactly that case.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::types::{AppState, err};
use crate::commands::setup::signin::{LiveAuthorizer, ProviderAuthorizer};

/// The seams and the shared state the provider routes need.
///
/// Shaped like [`McpAdmin`](super::mcp::McpAdmin) and for the same reason: the
/// live authorizer opens a browser and binds a fixed port, so a test supplies
/// its own rather than being careful.
#[derive(Clone)]
pub(crate) struct ProviderAdmin {
    /// Takes and forgets the sign-ins. Shared with `lev setup`, so the wizard
    /// and the API cannot disagree about where a grant goes.
    pub(crate) authorizer: Arc<LiveAuthorizer>,
    /// What each provider's sign-in is doing, for the poll to read.
    pub(crate) in_flight: Arc<Mutex<HashMap<String, Progress>>>,
    /// Current Unix time; a fn so a long-lived server stays current.
    pub(crate) now: fn() -> u64,
    /// Where the credential check reads the subscription's quota, when it is
    /// not the provider's own route.
    ///
    /// `None` in production. It exists so a test can answer the check without
    /// reaching OpenAI, and it is the same field anybody proxying that route
    /// would need.
    pub(crate) usage_url: Option<String>,
}

impl Default for ProviderAdmin {
    fn default() -> Self {
        Self {
            authorizer: Arc::new(LiveAuthorizer::real(
                Arc::new(leviath_sys::open_url),
                &super::mcp::admin_paths().config,
            )),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            now: super::mcp::system_now,
            usage_url: None,
        }
    }
}

/// Where one provider's sign-in has got to.
///
/// Only the unfinished states live here. A finished one is the grant store's
/// to report, and keeping a second copy of "signed in" is how the two come to
/// disagree.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Progress {
    /// The browser was asked to open this, and the callback has not arrived.
    Waiting {
        /// The page to open, for a host whose browser did not.
        authorize_url: String,
        /// When it started, in unix seconds.
        started_at: u64,
    },
    /// It did not finish, and this is why.
    Failed {
        /// What went wrong, ready to show.
        message: String,
        /// When it failed, in unix seconds.
        at: u64,
    },
}

/// One browser-sign-in provider, as the API reports it.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ProviderInfo {
    /// The registry name a blueprint would use: `codex/gpt-5.6-sol`.
    pub(crate) id: String,
    /// The name to show.
    pub(crate) display: String,
    /// Whether `config.toml` has it turned on. Separate from `signed_in`:
    /// the two are set by different routes and either can be true alone.
    pub(crate) enabled: bool,
    /// Whether a grant is stored.
    pub(crate) signed_in: bool,
    /// The account, when the grant names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    /// The subscription tier, when the grant names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<String>,
    /// When the *access* token lapses, in unix seconds.
    ///
    /// Not a deadline for the user: it is refreshed automatically well before
    /// this, and it is here so a console can show that the session is live
    /// rather than implying anybody has to act on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<u64>,
    /// A sign-in in flight, or the last one that failed. Absent when there is
    /// neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) signin: Option<serde_json::Value>,
}

/// The providers that sign in with a browser.
///
/// One entry today. A list rather than a `codex` key so a second one is a
/// table entry rather than a new route and a console change.
fn signin_providers() -> Vec<(&'static str, &'static str)> {
    crate::commands::setup::catalog::providers()
        .into_iter()
        .filter(|p| p.credential == crate::commands::setup::catalog::Credential::Signin)
        .map(|p| (p.id, p.display))
        .collect()
}

/// Describe one provider from the config, the grant store and the tracker.
fn describe(
    id: &str,
    display: &str,
    config: &crate::config::Config,
    store: Option<&leviath_providers::codex::ProviderAuthStore>,
    in_flight: &HashMap<String, Progress>,
) -> ProviderInfo {
    let grant = store.and_then(|store| store.get(id).cloned());
    let claims = grant.as_ref().map(leviath_providers::ProviderGrant::claims);
    ProviderInfo {
        id: id.to_string(),
        display: display.to_string(),
        enabled: config.providers.codex_enabled,
        signed_in: grant.is_some(),
        account: grant
            .as_ref()
            .and_then(|g| g.email.clone())
            .or_else(|| claims.as_ref().and_then(|c| c.email.clone())),
        plan: grant
            .as_ref()
            .and_then(|g| g.plan_type.clone())
            .or_else(|| claims.as_ref().and_then(|c| c.plan_type.clone())),
        expires_at: grant
            .as_ref()
            .and_then(|g| leviath_providers::codex::claims::expiry(&g.access_token)),
        signin: in_flight
            .get(id)
            .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null)),
    }
}

/// `GET /api/providers` - every browser-sign-in provider and its state.
pub(super) async fn list_providers(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.current_config();
    // Read once, not once per row: this is a file, and the answer is the same
    // for every provider in it.
    let store = state
        .providers
        .authorizer
        .store_path
        .as_deref()
        .and_then(|path| leviath_providers::codex::ProviderAuthStore::load(path).ok());
    let in_flight = leviath_core::sync::lock(&state.providers.in_flight).clone();
    let providers: Vec<ProviderInfo> = signin_providers()
        .into_iter()
        .map(|(id, display)| describe(id, display, &config, store.as_ref(), &in_flight))
        .collect();
    Json(serde_json::json!({ "providers": providers })).into_response()
}

/// The catalog's own id for `name`, or the refusal for a name nothing signs
/// in with.
///
/// Returns the `&'static str` from the table rather than the caller's string,
/// and every handler works from that. The two are equal by the time this
/// returns, so it is not a correctness fix - it is that nothing derived from
/// the request URL then reaches a credential store path, a registry entry or
/// a filesystem read, and neither a reader nor a scanner has to prove that by
/// following the string.
///
/// The refusal is boxed because an axum response is a large value and this
/// returns a small one beside it.
fn resolve(name: &str) -> Result<&'static str, Box<axum::response::Response>> {
    signin_providers()
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(id, _)| *id)
        .ok_or_else(|| {
            Box::new(
                err(
                    StatusCode::NOT_FOUND,
                    format!("no browser sign-in provider named '{name}'"),
                )
                .into_response(),
            )
        })
}

/// `POST /api/providers/{name}/login` - start the browser sign-in.
///
/// Returns `202` with the authorize URL as soon as there is one; the flow
/// continues behind the response and `GET /api/providers` reports how it went.
pub(super) async fn login(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let name = match resolve(&name) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    // One at a time: the flow owns a fixed loopback port that a second could
    // not bind, and two browser windows asking the same question help nobody.
    if let Some(Progress::Waiting { authorize_url, .. }) =
        leviath_core::sync::lock(&state.providers.in_flight).get(name)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("a sign-in to '{name}' is already waiting"),
                "authorize_url": authorize_url,
            })),
        )
            .into_response();
    }

    // One channel for both answers: the URL when the flow gets that far, and
    // the reason when it does not. A second channel, or reading the failure
    // back out of the tracker, would leave the handler racing the task that
    // recorded it.
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    // A `Fn`, not a `FnOnce`, so the sender lives in a slot it can be taken
    // out of. Whichever of the two fires first wins, and `login` announces at
    // most once.
    let slot = Arc::new(Mutex::new(Some(started_tx)));
    let announce_slot = Arc::clone(&slot);
    let announce: crate::commands::auth::codex::Announce = Arc::new(move |url: &str| {
        // `Option::map` rather than `if let`: an `if let` with no else leaves
        // a region only a second announce could reach, and there is not one.
        let _ = leviath_core::sync::lock(&announce_slot)
            .take()
            .map(|tx| tx.send(Ok(url.to_string())));
    });

    let authorizer = Arc::clone(&state.providers.authorizer);
    let tracker = Arc::clone(&state.providers.in_flight);
    let now = state.providers.now;
    tokio::spawn(async move {
        let outcome = authorizer.sign_in(name, announce).await;
        let mut in_flight = leviath_core::sync::lock(&tracker);
        match outcome {
            // Nothing is recorded on success: the grant store is now the
            // answer, and a second copy of "signed in" is how the two drift.
            Ok(_) => {
                in_flight.remove(name);
            }
            Err(e) => {
                let message = e
                    .chain()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(": ");
                // The handler is still waiting if this failed before there was
                // a URL to announce, and this is what it reads.
                let _ = leviath_core::sync::lock(&slot)
                    .take()
                    .map(|tx| tx.send(Err(message.clone())));
                in_flight.insert(name.to_string(), Progress::Failed { message, at: now() });
            }
        }
    });

    // Awaited without a deadline, deliberately. Everything between the spawn
    // above and one of the two answers is a PKCE generate, a loopback bind and
    // a string format: the URL exists within microseconds or the bind failed
    // and the reason is already on its way. A timeout here would be guarding
    // a stall that cannot happen, and the arm reporting it would be code no
    // test could ever reach.
    //
    // `unwrap_or` with a value rather than a closure covers the one case left:
    // a task that ended without answering either way, which is a panic in the
    // flow. The channel closes, and the caller is told rather than held.
    match started_rx
        .await
        .unwrap_or(Err("the sign-in ended before it began".to_string()))
    {
        Ok(url) => {
            leviath_core::sync::lock(&state.providers.in_flight).insert(
                name.to_string(),
                Progress::Waiting {
                    authorize_url: url.clone(),
                    started_at: (state.providers.now)(),
                },
            );
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "status": "waiting",
                    "provider": name,
                    "authorize_url": url,
                })),
            )
                .into_response()
        }
        Err(message) => err(StatusCode::BAD_GATEWAY, message).into_response(),
    }
}

/// `POST /api/providers/{name}/logout` - forget the stored grant.
///
/// `config.toml` is deliberately untouched, the same as `lev auth logout`:
/// signing out is not the same as turning the provider off, and doing both
/// would surprise anyone who meant to sign in again.
pub(super) async fn logout(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let name = match resolve(&name) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match state.providers.authorizer.sign_out(name).await {
        Ok(()) => {
            leviath_core::sync::lock(&state.providers.in_flight).remove(name);
            Json(serde_json::json!({ "status": "signed_out", "provider": name })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `POST /api/providers/{name}/check` - prove the stored sign-in works.
///
/// The same check `lev setup` runs, through the same code: it asks the account
/// rather than reading a compiled table, so a green answer here means the
/// subscription really did agree.
pub(super) async fn check(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let name = match resolve(&name) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let config = state.current_config();
    let mut options = crate::commands::run::session::codex_options(&config);
    // The authorizer's path, not the default one it usually resolves to: the
    // sign-in wrote there, and a check that read somewhere else would report
    // a provider with no grant a moment after storing one.
    options.extend(
        state
            .providers
            .authorizer
            .store_path
            .as_ref()
            .map(|path| ("auth_store_path".to_string(), path.display().to_string())),
    );
    options.extend(
        state
            .providers
            .usage_url
            .clone()
            .map(|url| ("usage_url".to_string(), url)),
    );
    let creds = leviath_runtime::provider_creds::ProviderCreds {
        // The table's id, not the caller's string: see `resolve`.
        name: name.to_string(),
        api_key: None,
        base_url: None,
        model_capabilities: HashMap::new(),
        request_timeout_secs: Some(20),
        rate_limit: None,
        options,
    };
    // Read through the outcome's own helpers rather than matched variant by
    // variant: `verify_via_registry` never skips - only the wizard's
    // `--no-verify` backend does, and that one is not wired here - so a
    // `Skipped` arm would be a branch nothing could reach.
    let outcome = crate::commands::setup::verify::verify_via_registry(&creds).await;
    if outcome.is_failure() {
        return err(StatusCode::BAD_GATEWAY, outcome.summary()).into_response();
    }
    Json(serde_json::json!({
        "status": "ok",
        "provider": name,
        "models": outcome.models(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests;
