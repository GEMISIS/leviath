//! The OpenAI Codex provider: inference billed to a ChatGPT subscription.
//!
//! A second, entirely separate door into OpenAI. The [`crate::OpenAIProvider`]
//! next to this one holds a static `sk-...` key and speaks Chat Completions at
//! `api.openai.com`, billing every token per use. This one holds a rotating
//! OAuth bearer obtained by signing in with a ChatGPT account, speaks the
//! Responses API at `chatgpt.com/backend-api/codex`, and spends the account's
//! subscription instead of an API balance.
//!
//! The two coexist deliberately: a user can hold both credentials, and which
//! one a stage uses is a routing decision, not a configuration accident.
//!
//! **`crate::openai_compat` is deliberately not reused.** The Responses API
//! (shared with the other providers that speak it, in [`crate::responses`])
//! differs from Chat Completions in the request root (`input` rather than
//! `messages`, `instructions` rather than a system message), in the streaming
//! event vocabulary, and in the usage field names. Forcing it through
//! `build_openai_request_body` would mean reshaping a module that three other
//! providers depend on, putting the risk on them rather than here.
//!
//! ## What the backend actually allows
//!
//! Measured against a live ChatGPT Plus account rather than taken from
//! documentation, because several widely-cited limits no longer hold:
//!
//! - `instructions` is free-form and large. It is not validated against a
//!   Codex base prompt, and 100 KB was accepted.
//! - `temperature` is rejected outright on every model, not merely on the
//!   handful that refuse a non-default value elsewhere.
//! - `max_output_tokens` is rejected. There is no wire field for an output cap,
//!   so a stage's cap cannot be enforced here.
//! - `store` must be `false`, which makes the backend stateless: reasoning
//!   continuity across turns depends on replaying an opaque
//!   `reasoning.encrypted_content` item, and a `function_call_output` whose
//!   matching `function_call` is missing is a hard 400.
//! - `response.completed` carries usage and status only. Its `output` array is
//!   always empty, so every output item has to be accumulated from the stream.

pub mod catalog;
pub mod headers;
pub mod provider;
pub mod usage;

pub use provider::CodexProvider;
pub use usage::{Quota, QuotaWindow};

/// The registry name this provider is known by, and the model prefix a
/// blueprint writes (`codex/gpt-5.6-sol`).
pub const PROVIDER_NAME: &str = "codex";

/// Where inference goes.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Where the subscription's quota windows are read from.
pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// The OAuth issuer for a ChatGPT account.
pub const ISSUER: &str = "https://auth.openai.com";

/// The public client id the ChatGPT sign-in flow uses.
///
/// Public in the OAuth sense: it identifies the application, carries no secret,
/// and is protected by PKCE. It is pre-registered, which is also why the
/// redirect port and path below are not ours to choose.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The only redirect port registered against [`CLIENT_ID`], with the one
/// fallback that is also registered.
///
/// Not a preference. A loopback server on any other port gets an authorization
/// request the issuer refuses to redirect to, so the usual "bind port zero and
/// take whatever the OS gives" pattern cannot be used here.
pub const CALLBACK_PORTS: [u16; 2] = [1455, 1457];

/// The redirect path registered against [`CLIENT_ID`].
pub const CALLBACK_PATH: &str = "/auth/callback";

/// The scopes the sign-in flow asks for.
pub const SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// How Leviath identifies itself to the backend.
///
/// The backend has been observed to whitelist this header, and third-party
/// clients sending their own name were reported to get 403s. Measured against
/// a live account, `leviath` is accepted, so Leviath says who it actually is
/// rather than dressing up as the Codex CLI. Overridable in config for the day
/// that changes.
pub const DEFAULT_ORIGINATOR: &str = "leviath";

/// How a ChatGPT account signs in.
///
/// Refresh is JSON where the code exchange is form-encoded, which is the
/// issuer's choice. The two authorize parameters put the workspace list in
/// the id token (where the account id the route wants comes from) and select
/// the Codex CLI's simplified consent page.
pub const PROFILE: crate::oauth::OAuthProfile = crate::oauth::OAuthProfile {
    provider: PROVIDER_NAME,
    brand: "ChatGPT",
    issuer: ISSUER,
    authorize_path: "/oauth/authorize",
    token_path: "/oauth/token",
    revoke_path: None,
    client_id: CLIENT_ID,
    scope: SCOPE,
    // The registered redirect is the literal string, and the issuer compares
    // it as one.
    redirect_host: "localhost",
    callback_ports: &CALLBACK_PORTS,
    callback_path: CALLBACK_PATH,
    refresh_body: crate::oauth::TokenBody::Json,
    authorize_params: &[
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", DEFAULT_ORIGINATOR),
    ],
    nonce: false,
    port_conflict: "The Codex CLI reserves the same ones: quit any `codex login` that is waiting and try again.",
};

/// This route's Responses rules, all measured against a live Plus account.
///
/// `temperature` and `max_output_tokens` are `400 Unsupported parameter` on
/// every model it serves, so a stage's output cap cannot be enforced here.
/// `prompt_cache_retention` is read-only (the server sets 24 hours).
pub const DIALECT: crate::responses::Dialect = crate::responses::Dialect {
    provider: PROVIDER_NAME,
    rejected_parameters: &[
        "temperature",
        "max_output_tokens",
        "max_tokens",
        "prompt_cache_retention",
    ],
    output_cap: false,
    temperature: false,
    verbosity: true,
    reasoning_summary: true,
    reported_cost: false,
    cache_key: true,
};
