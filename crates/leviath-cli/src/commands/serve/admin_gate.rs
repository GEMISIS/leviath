//! Who may use the admin routes and mutations, decided in one place.
//!
//! The admin surface changes the machine rather than a run: it writes the
//! config, installs scripts every agent then runs, adds MCP servers whose
//! commands Leviath spawns. A server offers it in one of three ways, fixed at
//! startup:
//!
//! - **Off**, the default. The REST routes are not mounted and the GraphQL
//!   mutations are hidden and refused.
//! - **Open**, with `--allow-admin` alone. Every holder of the API token may
//!   use them, for the life of the server.
//! - **Token**, with an admin token (`--admin-token` or
//!   `LEVIATH_API_ADMIN_TOKEN`). The routes are mounted, but only a request
//!   carrying the admin token may use them. A client keeps an ordinary session
//!   open and elevates only the writes its user approved.
//!
//! [`AdminGate::admits`] is the one decision. The REST layer
//! ([`require_admin`]) and the GraphQL guard both ask it, so a new admin route
//! or mutation is gated by being put where the others are.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use leviath_core::constant_time_eq;

/// The environment variable that sets the admin token.
pub(super) const ADMIN_TOKEN_ENV: &str = "LEVIATH_API_ADMIN_TOKEN";

/// The header a request names the admin token in, beside its ordinary bearer.
pub(super) const ADMIN_TOKEN_HEADER: &str = "x-leviath-admin-token";

/// What an admin request is refused with when the server wants the admin token.
///
/// Names where the token goes and never what it is.
pub(super) const ADMIN_TOKEN_REQUIRED: &str = "this server accepts admin requests only with its \
     admin token: send it as the bearer token, or in an X-Leviath-Admin-Token header beside the \
     API token";

/// How this server offers its admin surface. Decided once, at startup.
#[derive(Clone)]
pub(crate) enum AdminGate {
    /// No admin surface at all.
    Off,
    /// `--allow-admin`: every holder of the API token.
    Open,
    /// Only a request carrying this token.
    Token(Arc<String>),
}

impl AdminGate {
    /// Work out the gate from `--allow-admin`, `--admin-token` (`arg`) and the
    /// environment.
    ///
    /// An admin token always wins over `--allow-admin`: naming one is asking
    /// for the narrower promise, so passing both never widens it. A blank
    /// token, or one equal to the API token, refuses to start rather than
    /// quietly falling back to something broader than asked for.
    pub(super) fn resolve(
        allow_admin: bool,
        arg: Option<&str>,
        api_token: &str,
    ) -> anyhow::Result<Self> {
        let named = match arg {
            Some(value) => Some(value.to_string()),
            None => std::env::var(ADMIN_TOKEN_ENV).ok(),
        };
        let Some(token) = named.map(|value| value.trim().to_string()) else {
            return Ok(match allow_admin {
                true => Self::Open,
                false => Self::Off,
            });
        };
        anyhow::ensure!(
            !token.is_empty(),
            "the admin token is set but empty: give `--admin-token` or \
             {ADMIN_TOKEN_ENV} a value, or unset it"
        );
        anyhow::ensure!(
            !constant_time_eq(&token, api_token),
            "the admin token must differ from the API token, or every client \
             holding the API token could make admin requests"
        );
        Ok(Self::Token(Arc::new(token)))
    }

    /// Whether the admin routes are mounted and the mutations shown at all.
    pub(crate) fn mounted(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The admin token, when the server has one: it authenticates a request
    /// as well as elevating it.
    pub(super) fn token(&self) -> Option<Arc<String>> {
        match self {
            Self::Token(token) => Some(Arc::clone(token)),
            Self::Off | Self::Open => None,
        }
    }

    /// Whether a request presenting `presented` may make an admin request.
    ///
    /// The one place this is decided, for every admin route and mutation.
    pub(crate) fn admits(&self, presented: &Presented) -> bool {
        match self {
            Self::Off => false,
            Self::Open => true,
            Self::Token(token) => [&presented.bearer, &presented.elevation]
                .into_iter()
                .flatten()
                .any(|candidate| constant_time_eq(candidate, token)),
        }
    }
}

/// The credentials a request carried, as far as the admin gate cares.
///
/// Read off the request once, so the GraphQL guard can decide long after the
/// HTTP request has been taken apart. No `Debug`: it holds secrets.
#[derive(Clone, Default)]
pub(crate) struct Presented {
    /// What authenticated the request: the bearer, or `?token=` on a
    /// WebSocket route.
    bearer: Option<String>,
    /// The `X-Leviath-Admin-Token` header.
    elevation: Option<String>,
}

impl Presented {
    /// What a request with these headers and this address presents.
    pub(crate) fn of(headers: &HeaderMap, uri: &Uri) -> Self {
        Self {
            bearer: super::auth::presented_token(headers, uri),
            elevation: headers
                .get(ADMIN_TOKEN_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_string()),
        }
    }
}

/// Middleware over every admin route: through when the gate admits the
/// request, `403` otherwise.
pub(super) async fn require_admin(
    State(gate): State<AdminGate>,
    req: Request,
    next: Next,
) -> Response {
    match gate.admits(&Presented::of(req.headers(), req.uri())) {
        true => next.run(req).await,
        false => super::types::err(StatusCode::FORBIDDEN, ADMIN_TOKEN_REQUIRED.to_string())
            .into_response(),
    }
}

#[cfg(test)]
#[path = "admin_gate_tests.rs"]
mod tests;
