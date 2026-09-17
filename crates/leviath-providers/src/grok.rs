//! Grok billed to a subscription (SuperGrok, or X Premium+ on a linked X
//! account) rather than an xAI API balance.
//!
//! The inference side is the xAI provider itself ([`crate::xai`]), built over
//! a signed-in token instead of an API key: measured against a live
//! subscription, `https://api.x.ai/v1` accepts the OAuth bearer for every
//! route Leviath uses (listings, Responses, images) and bills the plan. This
//! module holds what is particular to the sign-in.
//!
//! **There is no published third-party client.** The sign-in uses the public
//! client id of xAI's own Grok CLI, the same one every other integration uses.
//! It carries no secret and is protected by PKCE. Leviath identifies itself as
//! Leviath on every request: the Grok CLI's own chat proxy
//! (`cli-chat-proxy.grok.com`) refuses inference unless the caller claims a
//! Grok CLI version, so Leviath never sends inference there, and only reads the
//! account routes on it that answer anyone signed in.

/// The registry name, and the model prefix a blueprint writes
/// (`grok/grok-4.6`).
pub const PROVIDER_NAME: &str = "grok";

/// The OAuth issuer for an xAI account.
pub const ISSUER: &str = "https://auth.x.ai";

/// The Grok CLI's public client id.
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// The redirect port registered against [`CLIENT_ID`].
pub const CALLBACK_PORTS: [u16; 1] = [56121];

/// The Grok CLI's account host. Read, never used for inference: see the
/// module documentation.
pub const ACCOUNT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

/// How an xAI account signs in. Measured 2026-09-16: the refresh must be
/// form-encoded (a JSON body is refused with 415), the id token echoes the
/// nonce, and access tokens last six hours.
pub const PROFILE: crate::oauth::OAuthProfile = crate::oauth::OAuthProfile {
    provider: PROVIDER_NAME,
    brand: "Grok",
    issuer: ISSUER,
    authorize_path: "/oauth2/authorize",
    token_path: "/oauth2/token",
    revoke_path: Some("/oauth2/revoke"),
    client_id: CLIENT_ID,
    scope: "openid profile email offline_access grok-cli:access api:access",
    redirect_host: "127.0.0.1",
    callback_ports: &CALLBACK_PORTS,
    callback_path: "/callback",
    refresh_body: crate::oauth::TokenBody::Form,
    authorize_params: &[("referrer", "leviath")],
    nonce: true,
    port_conflict: "The Grok CLI signs in on the same port: quit any `grok login` that is waiting and try again.",
};
