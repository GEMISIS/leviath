//! Signing in to a ChatGPT account, so Leviath can bill a subscription.
//!
//! An ordinary OAuth authorization-code flow with PKCE, with one constraint
//! that shapes the whole thing: the client id is pre-registered and public, so
//! the redirect URI is not ours to choose. The MCP login binds port zero and
//! takes whatever the OS gives, because an MCP server learns the redirect
//! through dynamic registration. Here only ports 1455 and 1457 are registered,
//! and the Codex CLI reserves the same two, so a collision is a likely outcome
//! rather than a remote one and the error has to say so.
//!
//! Leviath takes its own grant rather than reading `~/.codex/auth.json`.
//! Refresh tokens rotate and a spent one is terminal, so the first refresh
//! Leviath did would end the user's Codex CLI session. Two independent grants
//! on one client id are two independent rotation chains, which is safe.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leviath_providers::codex::{self, ProviderAuthStore, ProviderGrant};

/// How long to wait for the person to finish in the browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// How the authorize URL reaches whoever has to open it.
///
/// A closure rather than a `println!` because the wizard runs inside ratatui's
/// alternate screen, where printing either vanishes or corrupts the frame. The
/// command-line path prints; the wizard renders a selectable line.
pub type Announce = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything the flow needs that a test would rather supply itself.
pub struct LoginEnv {
    /// Opens the browser. Stubbed in tests so none ever launches.
    pub opener: leviath_mcp::BrowserOpener,
    /// Where the grant is written.
    pub store_path: PathBuf,
    /// The OS credential store, when one is configured.
    pub credential_store: Option<Arc<dyn leviath_core::CredentialStore>>,
    /// The outbound client.
    pub client: reqwest::Client,
    /// The OAuth issuer. Overridden in tests.
    pub issuer: String,
    /// How the authorize URL is shown.
    pub announce: Announce,
    /// The loopback ports to try, in order.
    pub ports: Vec<u16>,
}

impl LoginEnv {
    /// The production environment: the real browser, the real issuer, and the
    /// two ports the client id is registered against.
    pub fn new(
        opener: leviath_mcp::BrowserOpener,
        store_path: PathBuf,
        credential_store: Option<Arc<dyn leviath_core::CredentialStore>>,
        client: reqwest::Client,
        announce: Announce,
    ) -> Self {
        Self {
            opener,
            store_path,
            credential_store,
            client,
            issuer: codex::ISSUER.to_string(),
            announce,
            ports: codex::CALLBACK_PORTS.to_vec(),
        }
    }
}

/// Bind the first port the client id is registered against.
///
/// Both are tried because both are registered. Neither being available is a
/// real outcome rather than a defensive branch: the Codex CLI reserves exactly
/// these two for its own login, so the message names that.
async fn bind(ports: &[u16]) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    let mut last = String::new();
    for port in ports {
        match tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
            // The bound port, not the requested one. They are the same for the
            // registered ports and differ for port zero, and the redirect URI
            // has to name where the listener actually is.
            Ok(listener) => {
                let bound = listener.local_addr().map_or(*port, |addr| addr.port());
                return Ok((listener, bound));
            }
            Err(e) => last = e.to_string(),
        }
    }
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" or ");
    Err(anyhow::anyhow!(
        "could not listen on port {list} ({last}). The ChatGPT sign-in only redirects to \
         those ports, so this is not a port Leviath can choose. The Codex CLI reserves the \
         same ones: quit any `codex login` that is waiting and try again."
    ))
}

/// The URL to open in the browser.
fn authorize_url(issuer: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", codex::CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", codex::SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        // Puts the workspace list in the id token, which is where the account
        // id the inference route wants comes from.
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", codex::DEFAULT_ORIGINATOR)
        .finish();
    format!("{issuer}/oauth/authorize?{query}")
}

/// Exchange the authorization code for a grant.
///
/// Form-encoded, unlike the refresh, which is JSON. Both shapes are the
/// issuer's choice rather than a preference.
async fn exchange(
    client: &reqwest::Client,
    issuer: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> anyhow::Result<ProviderGrant> {
    let response = client
        .post(format!("{issuer}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", codex::CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach the ChatGPT sign-in service: {e}"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the sign-in was rejected (HTTP {status}): {body}");
    }

    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("the sign-in reply was not JSON: {e}"))?;
    let string = |key: &str| {
        parsed
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let id_token = string("id_token");
    let claims = codex::claims::parse(&id_token);
    let access_token = string("access_token");
    if access_token.is_empty() {
        anyhow::bail!("the sign-in reply carried no access token");
    }

    Ok(ProviderGrant {
        access_token,
        refresh_token: string("refresh_token"),
        id_token,
        account_id: claims.account_id,
        plan_type: claims.plan_type,
        email: claims.email,
    })
}

/// Run the whole flow and store the grant.
pub async fn login(env: &LoginEnv) -> anyhow::Result<ProviderGrant> {
    let pkce = leviath_mcp::Pkce::generate();
    let (listener, port) = bind(&env.ports).await?;
    // `localhost`, not `127.0.0.1`: the registered redirect is the literal
    // string, and the issuer compares it as one.
    let redirect_uri = format!("http://localhost:{port}{}", codex::CALLBACK_PATH);
    let url = authorize_url(&env.issuer, &redirect_uri, &pkce.challenge, &pkce.state);

    // Announced before the browser is asked to open it, so a headless or SSH
    // session still has something to copy when the opener does nothing.
    (env.announce)(&url);
    (env.opener)(&url);

    let code = leviath_mcp::wait_for_callback(
        listener,
        &pkce.state,
        codex::CALLBACK_PATH,
        CALLBACK_TIMEOUT,
    )
    .await?;

    let grant = exchange(
        &env.client,
        &env.issuer,
        &code,
        &redirect_uri,
        &pkce.verifier,
    )
    .await?;

    let store = env.credential_store.as_deref();
    let mut all = ProviderAuthStore::load_with(&env.store_path, store)?;
    all.set(codex::PROVIDER_NAME, grant.clone());
    all.save_with(&env.store_path, store)?;

    Ok(grant)
}

/// Forget the stored grant, reporting whether there was one.
///
/// `config.toml` is deliberately untouched: signing out is not the same as
/// disabling the provider, and silently doing both would surprise anyone who
/// meant to sign in again.
pub fn logout(
    store_path: &std::path::Path,
    credential_store: Option<&dyn leviath_core::CredentialStore>,
) -> anyhow::Result<bool> {
    let mut all = ProviderAuthStore::load_with(store_path, credential_store)?;
    let removed = all.remove(codex::PROVIDER_NAME);
    if removed {
        all.save_with(store_path, credential_store)?;
        if let Some(store) = credential_store {
            // The file's name index is gone; the OS entry has to go too, or a
            // later sign-in reads a grant nothing points at.
            let _ = store.delete(&codex::grant_account(codex::PROVIDER_NAME));
        }
    }
    Ok(removed)
}

// Visible to the rest of the crate's tests: `setup::signin` drives a whole
// sign-in through the same stub browser, and a second copy of that harness
// would be a second thing to keep true.
#[cfg(test)]
pub(crate) mod tests;
