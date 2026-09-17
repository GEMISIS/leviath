//! Browser sign-ins for providers billed to a subscription.
//!
//! Codex (a ChatGPT plan) and Grok (a SuperGrok or X Premium+ plan) both sign
//! in with an OAuth authorization-code flow and PKCE against a public client id
//! that is not Leviath's to register, and both hand back a short-lived bearer
//! with a rotating refresh token. Everything that follows from that is shared:
//! the grant store ([`store`]), the single-flight refresh ([`token`]), the
//! HTTP refresh ([`refresh`]) and the account claims ([`claims`]).
//!
//! What differs between issuers is data, and an [`OAuthProfile`] is that data.

pub mod claims;
pub mod refresh;
pub mod store;
pub mod token;

pub use claims::GrantClaims;
pub use refresh::HttpRefresh;
pub use store::{ProviderAuthStore, ProviderGrant, grant_account};
pub use token::{
    Credentials, OAuthTokenSource, RefreshError, RefreshTransport, RefreshedTokens, TokenSource,
};

/// How a token endpoint wants its refresh request encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenBody {
    /// `application/json`.
    Json,
    /// `application/x-www-form-urlencoded`.
    Form,
}

/// Everything about one issuer that a sign-in and a refresh need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthProfile {
    /// The provider's registry name, which is also the key its grant is
    /// stored under.
    pub provider: &'static str,
    /// The name a person knows the account by, for messages: "ChatGPT",
    /// "Grok".
    pub brand: &'static str,
    /// The OAuth issuer.
    pub issuer: &'static str,
    /// The authorization endpoint's path under [`Self::issuer`].
    pub authorize_path: &'static str,
    /// The token endpoint's path under [`Self::issuer`], for both the code
    /// exchange and the refresh.
    pub token_path: &'static str,
    /// The revocation endpoint's path, when the issuer has one. Signing out
    /// revokes the refresh token there as well as forgetting it.
    pub revoke_path: Option<&'static str>,
    /// The public client id. Carries no secret; PKCE protects the exchange.
    pub client_id: &'static str,
    /// The scopes asked for.
    pub scope: &'static str,
    /// The host in the redirect URI, exactly as the client id registered it.
    /// The issuer compares the whole string, so `localhost` and `127.0.0.1`
    /// are different redirects.
    pub redirect_host: &'static str,
    /// The loopback ports the client id is registered against, in order.
    pub callback_ports: &'static [u16],
    /// The redirect path registered against the client id.
    pub callback_path: &'static str,
    /// How the refresh request is encoded.
    pub refresh_body: TokenBody,
    /// Extra query pairs on the authorize URL.
    pub authorize_params: &'static [(&'static str, &'static str)],
    /// Whether to send an OpenID `nonce` and check the id token echoes it.
    pub nonce: bool,
    /// What else is likely holding the callback port, said when it is taken.
    pub port_conflict: &'static str,
}

impl OAuthProfile {
    /// The authorization endpoint, under `issuer` (the real one, or a test's).
    pub fn authorize_url(&self, issuer: &str) -> String {
        format!("{}{}", issuer.trim_end_matches('/'), self.authorize_path)
    }

    /// The token endpoint, under `issuer`.
    pub fn token_url(&self, issuer: &str) -> String {
        format!("{}{}", issuer.trim_end_matches('/'), self.token_path)
    }

    /// The revocation endpoint, under `issuer`, when there is one.
    pub fn revoke_url(&self, issuer: &str) -> Option<String> {
        self.revoke_path
            .map(|path| format!("{}{path}", issuer.trim_end_matches('/')))
    }

    /// The sentence that sends a person back to sign in.
    pub fn relogin_hint(&self) -> String {
        format!("run `lev auth login {}` to sign in again", self.provider)
    }
}

/// Every provider that signs in with a browser, by registry name.
pub fn profile(provider: &str) -> Option<&'static OAuthProfile> {
    [&crate::codex::PROFILE, &crate::grok::PROFILE]
        .into_iter()
        .find(|p| p.provider == provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_join_under_whichever_issuer_is_given() {
        let grok = profile("grok").unwrap();
        assert_eq!(
            grok.authorize_url("https://auth.x.ai/"),
            "https://auth.x.ai/oauth2/authorize"
        );
        assert_eq!(
            grok.token_url("http://127.0.0.1:9"),
            "http://127.0.0.1:9/oauth2/token"
        );
        assert_eq!(
            grok.revoke_url(grok.issuer).as_deref(),
            Some("https://auth.x.ai/oauth2/revoke")
        );
        let codex = profile("codex").unwrap();
        assert_eq!(codex.revoke_url(codex.issuer), None);
        assert!(codex.relogin_hint().contains("lev auth login codex"));
    }

    #[test]
    fn only_browser_sign_in_providers_have_a_profile() {
        assert!(profile("openai").is_none());
    }
}
