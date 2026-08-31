//! Taking a browser sign-in from inside the wizard.
//!
//! A provider whose credential is an OAuth grant has nothing for the wizard to
//! ask for. The first version of the Codex row said so and stopped there,
//! telling the user to quit and run `lev auth login codex` - which meant the
//! one provider that needs no typing was the only one setup could not finish.
//!
//! So the sign-in happens here instead, and `lev auth login` goes back to
//! being what it is actually for: signing in again on a machine with no
//! wizard, or after a session has been revoked.
//!
//! ## The seam
//!
//! [`ProviderAuthorizer`] exists for the same reason [`ProviderVerifier`] does,
//! with one addition: this one opens a browser. `lev dash` once had a unit test
//! launch a real one, and a test that reached this without a seam would open a
//! browser *and* wait five minutes for a callback nobody was going to make.
//! Nothing in the library builds the live implementation; the binary wires it
//! in, exactly as it does the verifier.
//!
//! [`ProviderVerifier`]: super::verify::ProviderVerifier

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::state::{SigninAction, SigninEvent, SigninRequest};
use crate::commands::auth::codex::{self as codex_login, LoginEnv};

/// Takes and forgets browser sign-ins.
pub trait ProviderAuthorizer {
    /// Sign `provider_id` in, reporting the authorize URL through `announce`
    /// as soon as there is one. Returns the identity line to show.
    ///
    /// `announce` fires before the browser is asked to open, so a session with
    /// no browser still has something to copy.
    fn sign_in(
        &self,
        provider_id: &str,
        announce: codex_login::Announce,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;

    /// Forget `provider_id`'s stored grant.
    fn sign_out(
        &self,
        provider_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// Production [`ProviderAuthorizer`]: really opens a browser and really writes
/// a grant.
///
/// Wired in only by the binary.
pub struct LiveAuthorizer {
    /// Opens the browser.
    pub opener: leviath_mcp::BrowserOpener,
    /// Where the grant is written. `None` when there is no home directory to
    /// write one into, which is the one case this cannot do anything about.
    pub store_path: Option<PathBuf>,
    /// The OS credential store `[security]` asked for, or why it could not be
    /// opened.
    ///
    /// Held unresolved rather than as an `Option`, so a keychain that cannot
    /// be reached is reported when the user presses sign in instead of being
    /// swallowed at start-up and writing the grant to a file they asked not to
    /// use.
    pub credential_store: Result<Option<Arc<dyn leviath_core::CredentialStore>>, String>,
    /// The outbound client the token exchange goes out on.
    ///
    /// From `leviath_net::client`, not the provider builder, for the reason
    /// `AuthEnv::real` gives: this is an OAuth exchange with an issuer rather
    /// than an inference call, so it takes the shared HTTP defaults and cannot
    /// fail to be built.
    pub client: reqwest::Client,
    /// The OAuth issuer, and the loopback ports its client id is registered
    /// against. Overridden only by tests, which point them at a local mock and
    /// port zero so a whole sign-in runs without a browser or a fixed port.
    pub issuer: String,
    /// See [`Self::issuer`].
    pub ports: Vec<u16>,
}

impl LiveAuthorizer {
    /// The authorizer for this machine: its home, and the credential backend
    /// `[security]` asked for.
    ///
    /// Assembled here rather than in the binary so the composition root stays
    /// one call, and because the config it reads is the same one the wizard is
    /// about to edit: a grant has to land wherever `credential_store` says,
    /// not wherever the default would have put it.
    #[must_use]
    pub fn real(opener: leviath_mcp::BrowserOpener, config_path: &std::path::Path) -> Self {
        let kind = crate::config::Config::load_from_path_public(config_path)
            .unwrap_or_default()
            .security
            .credential_store;
        Self {
            opener,
            store_path: leviath_providers::codex::ProviderAuthStore::default_path(),
            credential_store: crate::credentials::store_for(kind).map(|store| store.map(Arc::from)),
            client: leviath_net::client(leviath_net::ClientTimeouts::default()),
            issuer: leviath_providers::codex::ISSUER.to_string(),
            ports: leviath_providers::codex::CALLBACK_PORTS.to_vec(),
        }
    }

    /// The store path, or the error explaining why there is not one.
    fn store_path(&self) -> anyhow::Result<PathBuf> {
        self.store_path.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "no home directory to store the sign-in in; set LEVIATH_HOME and try again"
            )
        })
    }

    /// The credential store, or the reason it is unavailable.
    fn credential_store(&self) -> anyhow::Result<Option<Arc<dyn leviath_core::CredentialStore>>> {
        self.credential_store
            .clone()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Everything the flow needs, or the first reason it cannot run.
    ///
    /// Assembled before the flow starts so a missing home or an unreachable
    /// keychain is reported without a browser having opened onto a sign-in
    /// there is nowhere to store.
    fn login_env(&self, announce: codex_login::Announce) -> anyhow::Result<LoginEnv> {
        let mut env = LoginEnv::new(
            self.opener.clone(),
            self.store_path()?,
            self.credential_store()?,
            self.client.clone(),
            announce,
        );
        env.issuer = self.issuer.clone();
        env.ports = self.ports.clone();
        Ok(env)
    }
}

impl ProviderAuthorizer for LiveAuthorizer {
    async fn sign_in(
        &self,
        provider_id: &str,
        announce: codex_login::Announce,
    ) -> anyhow::Result<String> {
        unsupported(provider_id)?;
        let grant = codex_login::login(&self.login_env(announce)?).await?;
        Ok(describe(&grant))
    }

    async fn sign_out(&self, provider_id: &str) -> anyhow::Result<()> {
        unsupported(provider_id)?;
        codex_login::logout(&self.store_path()?, self.credential_store()?.as_deref())?;
        Ok(())
    }
}

/// Refuse a provider this does not know how to sign in.
///
/// Codex is the only one today. A second would be a match here rather than a
/// second implementation of the trait, since the wizard's side of it is the
/// same either way.
fn unsupported(provider_id: &str) -> anyhow::Result<()> {
    if provider_id == leviath_providers::codex::PROVIDER_NAME {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "'{provider_id}' does not sign in with a browser"
    ))
}

/// The identity line a signed-in grant shows as.
pub(crate) fn describe(grant: &leviath_providers::ProviderGrant) -> String {
    let claims = grant.claims();
    let who = grant
        .email
        .clone()
        .or(claims.email)
        .unwrap_or_else(|| "signed in".to_string());
    match grant.plan_type.clone().or(claims.plan_type) {
        Some(plan) => format!("{who} ({plan} plan)"),
        None => who,
    }
}

/// Serve sign-in requests until the wizard closes.
///
/// One at a time, and deliberately: a sign-in owns a fixed loopback port that
/// the second one could not bind, and two browser windows asking the same
/// question is not a thing to offer anybody.
pub async fn signin_loop<A: ProviderAuthorizer>(
    authorizer: A,
    mut requests: mpsc::UnboundedReceiver<SigninRequest>,
    events: mpsc::UnboundedSender<SigninEvent>,
) {
    while let Some(request) = requests.recv().await {
        let provider_id = request.provider_id.clone();
        let event = match request.action {
            SigninAction::In => {
                let announce = announcer(&events, &provider_id);
                match authorizer.sign_in(&provider_id, announce).await {
                    Ok(who) => SigninEvent::SignedIn { provider_id, who },
                    Err(e) => failed(provider_id, &e),
                }
            }
            SigninAction::Out => match authorizer.sign_out(&provider_id).await {
                Ok(()) => SigninEvent::SignedOut { provider_id },
                Err(e) => failed(provider_id, &e),
            },
        };
        // A closed receiver means the wizard exited; nothing left to report to.
        if events.send(event).is_err() {
            return;
        }
    }
}

/// The callback that forwards an authorize URL to the wizard.
fn announcer(
    events: &mpsc::UnboundedSender<SigninEvent>,
    provider_id: &str,
) -> codex_login::Announce {
    let events = events.clone();
    let provider_id = provider_id.to_string();
    Arc::new(move |url: &str| {
        let _ = events.send(SigninEvent::Opened {
            provider_id: provider_id.clone(),
            url: url.to_string(),
        });
    })
}

/// One failure, reported as the card's message.
///
/// The whole chain, because the useful half is usually the cause: "could not
/// listen on port 1455 or 1457" is what the user has to act on, and the
/// outer sentence alone would not say it.
fn failed(provider_id: String, error: &anyhow::Error) -> SigninEvent {
    let message = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    SigninEvent::Failed {
        provider_id,
        message,
    }
}

#[cfg(test)]
mod tests;
