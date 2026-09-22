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
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::quota_cache::{Accounts, Asked, QUOTA_AGE, QUOTA_COMPLETE};
use super::types::AppState;
use crate::commands::setup::signin::{LiveAuthorizer, ProviderAuthorizer};

/// The seams and the shared state the provider routes need.
///
/// Shaped like [`McpAdmin`](super::mcp::McpAdmin) and for the same reason: the
/// live authorizer opens a browser and binds a fixed port, so a test supplies
/// its own rather than being careful.
///
/// **No file location lives here**, for the reason [`AdminPaths`] gives:
/// anything reachable from a handler's parameters is request data as far as a
/// scanner is concerned, and a file location that is request data is a
/// path-injection finding. The grant store comes from
/// [`admin_paths`](super::mcp::admin_paths) inside each handler instead.
///
/// [`AdminPaths`]: super::mcp::AdminPaths
#[derive(Clone)]
pub(crate) struct ProviderAdmin {
    /// Opens the browser during a sign-in.
    pub(crate) opener: leviath_mcp::BrowserOpener,
    /// The OAuth issuer, and the loopback ports its client id is registered
    /// against. Overridden only by tests, which point them at a local mock and
    /// port zero so a whole sign-in runs without a browser or a fixed port;
    /// `None` is each provider's own.
    pub(crate) issuer: Option<String>,
    /// See [`Self::issuer`].
    pub(crate) ports: Option<Vec<u16>>,
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
            opener: Arc::new(leviath_sys::open_url),
            issuer: None,
            ports: None,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            now: super::mcp::system_now,
            usage_url: None,
        }
    }
}

impl ProviderAdmin {
    /// The authorizer for this request.
    ///
    /// Built per call rather than held, so the grant store and the credential
    /// backend are both read from where they are *now*, and a `[security]
    /// credential_store` change takes effect without restarting `lev serve`.
    fn authorizer(&self) -> LiveAuthorizer {
        let paths = super::mcp::admin_paths();
        let mut authorizer = LiveAuthorizer::real(self.opener.clone(), &paths.config);
        authorizer.store_path = Some(paths.grants);
        authorizer.issuer = self.issuer.clone();
        authorizer.ports = self.ports.clone();
        authorizer
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
    /// What the subscription has left, read live from the account, when the
    /// request asked for it with `?quota=true` and the provider is enabled
    /// and signed in: `{"report": {...}}` or `{"error": "..."}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) quota: Option<serde_json::Value>,
}

/// `GET /api/providers` query parameters.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ListQuery {
    /// Read each signed-in subscription's usage too. Off by default: it is a
    /// network read per provider, and a console polls this route while a
    /// sign-in is waiting.
    #[serde(default, deserialize_with = "super::types::flag")]
    pub(super) quota: bool,
    /// Ask the accounts again and wait for them, rather than answering from
    /// the reading the server keeps. Only meaningful beside `quota`; for a
    /// console's "check again".
    #[serde(default, deserialize_with = "super::types::flag")]
    pub(super) refresh: bool,
}

/// The providers that sign in with a browser, as the setup catalog lists them.
///
/// A list rather than one key per provider, so each is a table entry rather
/// than a new route and a console change.
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
    store: Option<&leviath_providers::oauth::ProviderAuthStore>,
    in_flight: &HashMap<String, Progress>,
) -> ProviderInfo {
    let grant = store.and_then(|store| store.get(id).cloned());
    let claims = grant.as_ref().map(leviath_providers::ProviderGrant::claims);
    ProviderInfo {
        id: id.to_string(),
        display: display.to_string(),
        enabled: crate::commands::setup::catalog::signin_enabled(config, id),
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
            .and_then(|g| leviath_providers::oauth::claims::expiry(&g.access_token)),
        signin: in_flight
            .get(id)
            .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null)),
        quota: None,
    }
}

/// `GET /api/providers` - every browser-sign-in provider and its state.
pub(super) async fn list_providers(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> impl IntoResponse {
    listing_with(&state, &query).await
}

/// Every provider this machine can reach, with what is configured and what is
/// signed in. No quota: reading that costs a provider call, so it is asked for
/// separately.
///
/// Both surfaces build their rows here, so "which providers are there" cannot
/// have two answers.
pub(super) fn provider_infos(state: &AppState) -> Vec<ProviderInfo> {
    let config = state.current_config();
    // Read once, not once per row: this is a file, and the answer is the same
    // for every provider in it.
    let paths = super::mcp::admin_paths();
    let store = leviath_providers::oauth::ProviderAuthStore::load(&paths.grants).ok();
    let in_flight = leviath_core::sync::lock(&state.providers.in_flight).clone();
    signin_providers()
        .into_iter()
        .map(|(id, display)| describe(id, display, &config, store.as_ref(), &in_flight))
        .collect()
}

/// [`list_providers`], callable from a test without a request.
///
/// With `?quota=true` the answer carries `X-Leviath-Quota-Age` and
/// `X-Leviath-Quota-Complete`; without it neither, since an age describing a
/// reading that is not in the response is worse than no header at all.
pub(super) async fn listing_with(
    state: &AppState,
    query: &ListQuery,
) -> (HeaderMap, Json<serde_json::Value>) {
    let config = state.current_config();
    let mut providers: Vec<ProviderInfo> = provider_infos(state);
    let mut headers = HeaderMap::new();
    if query.quota {
        // Built from the rows above rather than from a second look at the
        // grant store, so what the reading is keyed on and what the body says
        // about who is signed in cannot come apart.
        let asked = providers
            .iter()
            .filter(|p| p.enabled && p.signed_in)
            .map(|p| Asked {
                id: p.id.clone(),
                account: p.account.clone(),
            })
            .collect();
        // The grant store's location comes from `admin_paths` rather than from
        // `state`; see `ProviderAdmin`.
        let grants = super::mcp::admin_paths().grants;
        let (reading, _) = state
            .caches
            .provider_quota
            .report(Accounts::new(config, grants, asked), query.refresh)
            .await;
        let mut read: HashMap<&str, serde_json::Value> = reading
            .value
            .iter()
            .map(|usage| {
                (
                    usage.provider,
                    crate::commands::providers::quota::entry(usage),
                )
            })
            .collect();
        for provider in &mut providers {
            provider.quota = read.remove(provider.id.as_str());
        }
        headers.insert(QUOTA_AGE, HeaderValue::from(reading.age_secs()));
        headers.insert(
            QUOTA_COMPLETE,
            HeaderValue::from_static(if reading.complete { "true" } else { "false" }),
        );
    }
    (headers, Json(serde_json::json!({ "providers": providers })))
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
    canonical(name)
        .map(|(id, _)| id)
        .map_err(|e| Box::new(super::core::error::as_api_error(&e).into_response()))
}

/// The catalog's own entry for a provider that can be signed in to in a
/// browser: its id and the name to show.
///
/// The table's own id, not the caller's string: everything downstream keys on
/// the id, and a caller's spelling that merely matched would key a grant under a
/// name nothing reads back. The display name comes with it so a caller that has
/// to describe the provider has no second lookup to make, and so no miss to
/// report about a provider this one already vouched for.
pub(super) fn canonical(
    name: &str,
) -> Result<(&'static str, &'static str), super::core::error::ServeError> {
    signin_providers()
        .iter()
        .find(|(id, _)| *id == name)
        .copied()
        .ok_or_else(|| {
            super::core::error::ServeError::NotFound(format!(
                "no browser sign-in provider named '{name}'"
            ))
        })
}

/// One provider's row, for a name [`canonical`] has already vouched for.
///
/// [`provider_infos`] for one provider rather than all of them, built from the
/// catalog entry the caller is holding: there is nothing to search, and so no
/// absent case to invent an answer for.
pub(super) fn described(state: &AppState, id: &str, display: &str) -> ProviderInfo {
    let config = state.current_config();
    let paths = super::mcp::admin_paths();
    let store = leviath_providers::oauth::ProviderAuthStore::load(&paths.grants).ok();
    let in_flight = leviath_core::sync::lock(&state.providers.in_flight).clone();
    describe(id, display, &config, store.as_ref(), &in_flight)
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
    match sign_in_started(&state, name).await {
        // Already waiting: the same 409 as before, carrying the URL, because a
        // client that asked twice still needs the window it is waiting on.
        Ok(started) if started.already_waiting => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("a sign-in to '{name}' is already waiting"),
                "authorize_url": started.authorize_url,
            })),
        )
            .into_response(),
        Ok(started) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "waiting",
                "provider": started.provider,
                "authorize_url": started.authorize_url,
            })),
        )
            .into_response(),
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// A sign-in that is waiting for the person to finish it in a browser.
#[derive(Debug, Clone)]
pub(super) struct SignInStarted {
    /// The provider, by its canonical name.
    pub(super) provider: String,
    /// Where the person has to go. On the serving host: the flow listens on a
    /// loopback port there, so a browser anywhere else cannot complete it.
    pub(super) authorize_url: String,
    /// Whether this is the sign-in somebody already started rather than a new
    /// one. One runs at a time, because the flow owns a fixed loopback port that
    /// a second could not bind, and two browser windows asking the same question
    /// help nobody.
    pub(super) already_waiting: bool,
}

/// Start a sign-in, for whichever surface asked, and answer with the URL.
///
/// Returns as soon as there is a URL to go to. What happens after that is the
/// person's business and the flow's: read `providers` to see whether it landed.
pub(super) async fn sign_in_started(
    state: &AppState,
    name: &'static str,
) -> Result<SignInStarted, super::core::error::ServeError> {
    // One at a time: the flow owns a fixed loopback port that a second could
    // not bind, and two browser windows asking the same question help nobody.
    if let Some(Progress::Waiting { authorize_url, .. }) =
        leviath_core::sync::lock(&state.providers.in_flight).get(name)
    {
        return Ok(SignInStarted {
            provider: name.to_string(),
            authorize_url: authorize_url.clone(),
            already_waiting: true,
        });
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
    let announce: crate::commands::auth::oauth::Announce = Arc::new(move |url: &str| {
        // `Option::map` rather than `if let`: an `if let` with no else leaves
        // a region only a second announce could reach, and there is not one.
        let _ = leviath_core::sync::lock(&announce_slot)
            .take()
            .map(|tx| tx.send(Ok(url.to_string())));
    });

    // Resolved here, in the request, and moved into the task below: a
    // `tokio::spawn` does not inherit the task-local the tests scope, so
    // resolving it in there would reach the real home.
    let authorizer = state.providers.authorizer();
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
            Ok(SignInStarted {
                provider: name.to_string(),
                authorize_url: url,
                already_waiting: false,
            })
        }
        Err(message) => Err(super::core::error::ServeError::Upstream(message)),
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
    match signed_out(&state, name).await {
        Ok(()) => {
            Json(serde_json::json!({ "status": "signed_out", "provider": name })).into_response()
        }
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// Forget one provider's stored grant, for whichever surface asked.
///
/// The config is deliberately untouched, the same as `lev auth logout`: signing
/// out is not turning the provider off, and doing both would surprise anybody who
/// meant to sign in again.
pub(super) async fn signed_out(
    state: &AppState,
    name: &str,
) -> Result<(), super::core::error::ServeError> {
    state
        .providers
        .authorizer()
        .sign_out(name)
        .await
        .map_err(|e| super::core::error::ServeError::Internal(e.to_string()))?;
    leviath_core::sync::lock(&state.providers.in_flight).remove(name);
    Ok(())
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
    match checked(&state, name).await {
        Ok(models) => Json(serde_json::json!({
            "status": "ok",
            "provider": name,
            "models": models,
        }))
        .into_response(),
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// Ask one provider whether the stored sign-in works, for whichever surface
/// asked.
///
/// The same check `lev setup` runs, through the same code: it asks the account
/// rather than reading a compiled table, so a green answer means the subscription
/// really did agree. The models it names are what that account may use.
pub(super) async fn checked(
    state: &AppState,
    name: &str,
) -> Result<Vec<String>, super::core::error::ServeError> {
    let config = state.current_config();
    let mut options = crate::commands::run::session::signin_options(&config, name);
    // The authorizer's path, not the default one it usually resolves to: the
    // sign-in wrote there, and a check that read somewhere else would report
    // a provider with no grant a moment after storing one.
    options.insert(
        "auth_store_path".to_string(),
        super::mcp::admin_paths().grants.display().to_string(),
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
    match outcome.is_failure() {
        true => Err(super::core::error::ServeError::Upstream(outcome.summary())),
        false => Ok(outcome.models().to_vec()),
    }
}

#[cfg(test)]
mod tests;
