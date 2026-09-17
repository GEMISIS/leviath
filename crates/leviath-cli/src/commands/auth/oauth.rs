//! Signing in to a subscription account, so Leviath can bill a plan rather
//! than an API balance.
//!
//! An ordinary OAuth authorization-code flow with PKCE, with one constraint
//! that shapes the whole thing: each client id is pre-registered and public,
//! so the redirect URI is not ours to choose. The MCP login binds port zero and
//! takes whatever the OS gives, because an MCP server learns the redirect
//! through dynamic registration. Here only the ports the issuer registered are
//! any use, and the vendor's own CLI reserves the same ones, so a collision is
//! a likely outcome rather than a remote one and the error has to say so.
//!
//! What differs between issuers lives in an
//! [`leviath_providers::oauth::OAuthProfile`].
//!
//! Leviath takes its own grant rather than reading the vendor CLI's session
//! file (`~/.codex/auth.json`, `~/.grok/auth.json`). Refresh tokens rotate, so
//! the first refresh Leviath did could end the user's CLI session. Two
//! independent grants on one client id are two independent rotation chains,
//! which is safe.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leviath_providers::oauth::{OAuthProfile, ProviderAuthStore, ProviderGrant};

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
    /// Which issuer, and how it wants to be asked.
    pub profile: &'static OAuthProfile,
    /// Opens the browser. Stubbed in tests so none ever launches.
    pub opener: leviath_mcp::BrowserOpener,
    /// Where the grant is written.
    pub store_path: PathBuf,
    /// The OS credential store, when one is configured.
    pub credential_store: Option<Arc<dyn leviath_core::CredentialStore>>,
    /// The outbound client.
    pub client: reqwest::Client,
    /// The OAuth issuer. The profile's own outside tests.
    pub issuer: String,
    /// How the authorize URL is shown.
    pub announce: Announce,
    /// The loopback ports to try, in order.
    pub ports: Vec<u16>,
}

impl LoginEnv {
    /// The production environment for `profile`: the real browser, the real
    /// issuer, and the ports the client id is registered against.
    pub fn new(
        profile: &'static OAuthProfile,
        opener: leviath_mcp::BrowserOpener,
        store_path: PathBuf,
        credential_store: Option<Arc<dyn leviath_core::CredentialStore>>,
        client: reqwest::Client,
        announce: Announce,
    ) -> Self {
        Self {
            profile,
            opener,
            store_path,
            credential_store,
            client,
            issuer: profile.issuer.to_string(),
            announce,
            ports: profile.callback_ports.to_vec(),
        }
    }
}

/// Bind the first port the client id is registered against.
///
/// Every registered port is tried. None being available is a real outcome
/// rather than a defensive branch: the vendor's own CLI reserves exactly these
/// for its login, so the message names that.
async fn bind(
    profile: &OAuthProfile,
    ports: &[u16],
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
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
        "could not listen on port {list} ({last}). The {} sign-in only redirects to \
         that port, so this is not a port Leviath can choose. {}",
        profile.brand,
        profile.port_conflict
    ))
}

/// The URL to open in the browser.
fn authorize_url(
    profile: &OAuthProfile,
    issuer: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    nonce: Option<&str>,
) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("response_type", "code")
        .append_pair("client_id", profile.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", profile.scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    for (key, value) in profile.authorize_params {
        query.append_pair(key, value);
    }
    if let Some(nonce) = nonce {
        query.append_pair("nonce", nonce);
    }
    format!("{}?{}", profile.authorize_url(issuer), query.finish())
}

/// Exchange the authorization code for a grant.
///
/// Form-encoded, which every issuer here accepts for the code exchange.
async fn exchange(
    env: &LoginEnv,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    nonce: Option<&str>,
) -> anyhow::Result<ProviderGrant> {
    let profile = env.profile;
    let response = env
        .client
        .post(profile.token_url(&env.issuer))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", profile.client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!("could not reach the {} sign-in service: {e}", profile.brand)
        })?;

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
    // The id token has to carry the nonce this flow sent. A reply that does not
    // is not the answer to this sign-in, whatever else it holds.
    if let Some(sent) = nonce
        && leviath_providers::oauth::claims::nonce(&id_token).as_deref() != Some(sent)
    {
        anyhow::bail!("the sign-in reply was not issued for this sign-in (nonce mismatch)");
    }
    let claims = leviath_providers::oauth::claims::parse(&id_token);
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
    let profile = env.profile;
    let pkce = leviath_mcp::Pkce::generate();
    // Another PKCE draw's state, which is the same high-entropy random string a
    // nonce needs to be.
    let nonce = profile.nonce.then(|| leviath_mcp::Pkce::generate().state);
    let (listener, port) = bind(profile, &env.ports).await?;
    let redirect_uri = format!(
        "http://{}:{port}{}",
        profile.redirect_host, profile.callback_path
    );
    let url = authorize_url(
        profile,
        &env.issuer,
        &redirect_uri,
        &pkce.challenge,
        &pkce.state,
        nonce.as_deref(),
    );

    // Announced before the browser is asked to open it, so a headless or SSH
    // session still has something to copy when the opener does nothing.
    (env.announce)(&url);
    (env.opener)(&url);

    let code = leviath_mcp::wait_for_callback(
        listener,
        &pkce.state,
        profile.callback_path,
        CALLBACK_TIMEOUT,
    )
    .await?;

    let grant = exchange(env, &code, &redirect_uri, &pkce.verifier, nonce.as_deref()).await?;

    let store = env.credential_store.as_deref();
    let mut all = ProviderAuthStore::load_with(&env.store_path, store)?;
    all.set(profile.provider, grant.clone());
    all.save_with(&env.store_path, store)?;

    Ok(grant)
}

/// Forget `profile`'s stored grant, reporting whether there was one.
///
/// `config.toml` is deliberately untouched: signing out is not the same as
/// disabling the provider, and silently doing both would surprise anyone who
/// meant to sign in again.
pub fn logout(
    profile: &OAuthProfile,
    store_path: &std::path::Path,
    credential_store: Option<&dyn leviath_core::CredentialStore>,
) -> anyhow::Result<bool> {
    let mut all = ProviderAuthStore::load_with(store_path, credential_store)?;
    let removed = all.remove(profile.provider);
    if removed {
        all.save_with(store_path, credential_store)?;
        if let Some(store) = credential_store {
            // The file's name index is gone; the OS entry has to go too, or a
            // later sign-in reads a grant nothing points at.
            let _ = store.delete(&leviath_providers::oauth::grant_account(profile.provider));
        }
    }
    Ok(removed)
}

/// Revoke the stored refresh token at the issuer, when the issuer has a
/// revocation endpoint and a grant is stored.
///
/// Best effort, and done before the grant is forgotten: a refresh token that
/// is only deleted locally still works for anyone holding a copy until it
/// expires. A failure here never stops the sign-out; it is returned so the
/// caller can say so.
pub async fn revoke(
    profile: &OAuthProfile,
    client: &reqwest::Client,
    issuer: &str,
    store_path: &std::path::Path,
    credential_store: Option<&dyn leviath_core::CredentialStore>,
) -> Result<(), String> {
    let Some(url) = profile.revoke_url(issuer) else {
        return Ok(());
    };
    let grant = ProviderAuthStore::load_with(store_path, credential_store)
        .map_err(|e| e.to_string())?
        .get(profile.provider)
        .cloned();
    let Some(grant) = grant else {
        return Ok(());
    };
    let response = client
        .post(url)
        .form(&[
            ("token", grant.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", profile.client_id),
        ])
        .send()
        .await
        .map_err(|e| format!("could not reach the issuer to revoke the session: {e}"))?;
    match response.status().is_success() {
        true => Ok(()),
        false => Err(format!(
            "the issuer did not revoke the session (HTTP {})",
            response.status()
        )),
    }
}

// Visible to the rest of the crate's tests: `setup::signin` drives a whole
// sign-in through the same stub browser, and a second copy of that harness
// would be a second thing to keep true.
#[cfg(test)]
pub(crate) mod tests;
