//! Choosing and using the configured credential backend.
//!
//! [`leviath_core::credentials`] defines the vocabulary and the
//! [`CredentialStore`] trait; `leviath_sys::keychain` owns the OS binding and
//! its no-store fallback. This module is the seam between them: it turns a
//! `[security] credential_store` setting into something the config loader,
//! the `lev auth` command and every MCP login surface can call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leviath_core::{CredentialStore, CredentialStoreKind};
use leviath_mcp::AuthStore;

/// A [`CredentialStore`] backed by the OS credential store.
///
/// A thin adapter over `leviath_sys::keychain`: the platform work, the feature
/// gate, and the "no store available" fallback all live there, so this is only
/// the trait impl that lets the rest of the CLI stay generic over the backend.
pub(crate) struct KeychainStore {
    service: String,
}

impl KeychainStore {
    /// A store filing credentials under `service`.
    ///
    /// Availability is [`store_for`]'s job, not this one's: it probes once and
    /// reports a missing keychain there, rather than letting every individual
    /// key read fail separately with an error that reads like a missing key.
    pub(crate) fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }
}

impl CredentialStore for KeychainStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        leviath_sys::keychain::get(&self.service, account)
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        leviath_sys::keychain::set(&self.service, account, secret)
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        leviath_sys::keychain::delete(&self.service, account)
    }
}

/// The store named by `kind`, or `None` when secrets belong in Leviath's own
/// files.
///
/// `None` is the ordinary answer, not a failure: `file` is the default backend.
pub fn store_for(kind: CredentialStoreKind) -> Resolved {
    #[cfg(test)]
    if test_store::UNREACHABLE.load(std::sync::atomic::Ordering::SeqCst) {
        return store_for_with(kind, no_store_available);
    }
    store_for_with(kind, leviath_sys::keychain::probe)
}

/// The resolved backend: `Ok(None)` for the file store, `Ok(Some(_))` for a
/// working keychain, `Err` for a keychain that was asked for but is unreachable.
pub(crate) type Resolved = Result<Option<Box<dyn CredentialStore>>, String>;

/// Core of [`store_for`] with the availability check injected.
///
/// A `fn` pointer (not `impl Fn`) so there is one monomorphization, matching the
/// seam idiom used for the browser opener and the socket peer lookup. The seam
/// is not a convenience: "no store is installed in this process" and "this
/// machine has no credential store" are different things, and on a developer's
/// Mac the first silently becomes the second - the real probe would install the
/// platform store and every following operation would hit the real login
/// keychain, prompting and writing. Injecting the probe is what makes an
/// unavailable keychain testable without that.
fn store_for_with(kind: CredentialStoreKind, probe: fn(&str) -> Result<(), String>) -> Resolved {
    match kind {
        CredentialStoreKind::File => Ok(None),
        CredentialStoreKind::Keychain => {
            let service = leviath_core::credentials::SERVICE;
            probe(service).map_err(|e| {
                format!("`[security] credential_store = \"keychain\"` is set, but {e}")
            })?;
            Ok(Some(Box::new(KeychainStore::new(service))))
        }
    }
}

/// Where MCP OAuth grants are kept: the grant file, and under `[security]
/// credential_store = "keychain"` the OS store holding the tokens the file
/// only names.
///
/// One value for the pair, because a grant has to be read and written through
/// the backend that holds it. Read through the file alone, a keychain user's
/// login shows as logged out; written through it, their refresh token lands
/// on disk.
///
/// A keychain that was asked for and cannot be reached is kept here and given
/// back by every load, save and forget, so each surface reports it where it
/// reports a grant file that will not load, and no write lands in the file
/// instead. The readers that save nothing read the file alone: an agent's
/// connections through [`at_or_file`](Self::at_or_file), a listing through
/// [`shown`](Self::shown).
#[derive(Clone)]
pub struct McpGrants {
    path: PathBuf,
    store: Result<Option<Arc<dyn CredentialStore>>, String>,
}

impl McpGrants {
    /// The grants in `path`, kept where `kind` says.
    pub fn at(path: PathBuf, kind: CredentialStoreKind) -> Self {
        Self::new(path, store_for(kind))
    }

    /// [`at`](Self::at) with the backend already resolved.
    pub(crate) fn new(path: PathBuf, resolved: Resolved) -> Self {
        Self {
            path,
            store: resolved.map(|store| store.map(Arc::from)),
        }
    }

    /// The grants in `path`, kept where `kind` says, or in the file alone when
    /// the keychain cannot be reached.
    ///
    /// For connecting on someone's behalf: an agent's MCP servers, the tools
    /// the agent editor offers. A warning rather than a failure, because
    /// servers that need no OAuth still work, and refusing every one over a
    /// locked keychain would be worse than losing the ones that need it. The
    /// file alone holds none of a keychain user's tokens, so those servers
    /// read as logged out and nothing writes a token into the file.
    pub fn at_or_file(path: PathBuf, kind: CredentialStoreKind) -> Self {
        Self::or_file(path, store_for(kind))
    }

    /// [`at_or_file`](Self::at_or_file) with the backend already resolved.
    pub(crate) fn or_file(path: PathBuf, resolved: Resolved) -> Self {
        let resolved = resolved.or_else(|e| {
            tracing::warn!("{e}. MCP servers needing OAuth will appear logged out.");
            Ok(None)
        });
        Self::new(path, resolved)
    }

    /// The grant file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The store holding the tokens, `None` for the file itself.
    fn store(&self) -> anyhow::Result<Option<&dyn CredentialStore>> {
        self.store
            .as_ref()
            .map(Option::as_deref)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Every grant, or none when there is no file yet.
    ///
    /// A file that will not load is an error rather than an empty set: it
    /// holds every server's grant, and a save over an empty set would lose
    /// them all.
    pub fn load(&self) -> anyhow::Result<AuthStore> {
        AuthStore::load_with(&self.path, self.store()?)
    }

    /// Write `grants` back through the backend they belong in.
    pub fn save(&self, grants: &AuthStore) -> anyhow::Result<()> {
        grants.save_with(&self.path, self.store()?)
    }

    /// Every grant a listing shows, and why it could not show them, when it
    /// could not.
    ///
    /// Never a failure: a listing writes nothing, so a server shown not logged
    /// in loses nothing, where a failed listing hides every server, the ones
    /// that need no grant with them. A keychain that cannot be reached leaves
    /// the file alone to read, as [`at_or_file`](Self::at_or_file) does; a
    /// file that will not load leaves nothing. A save still refuses both.
    pub fn shown(&self) -> (AuthStore, Option<String>) {
        let (store, unreached) = match &self.store {
            Ok(store) => (store.as_deref(), None),
            Err(e) => (None, Some(e.clone())),
        };
        match AuthStore::load_with(&self.path, store) {
            Ok(held) => (held, unreached),
            Err(e) => (AuthStore::default(), Some(e.to_string())),
        }
    }

    /// Forget `server`'s grant, reporting whether it had one.
    ///
    /// Under the keychain the secret is deleted before the file stops naming
    /// it: a delete that fails leaves the name for the next attempt to find,
    /// where the other order would strand a refresh token nothing points at.
    /// A secret the file does not name is deleted too, so a stranded one has
    /// a way out.
    pub fn forget(&self, server: &str) -> anyhow::Result<bool> {
        let store = self.store()?;
        let mut grants = AuthStore::load_with(&self.path, store)?;
        let named = grants.remove(server);
        let deleted = match store {
            Some(store) => store
                .delete(&leviath_core::mcp_account(server))
                .map_err(|e| anyhow::anyhow!("could not forget the login for '{server}': {e}"))?,
            None => false,
        };
        if named {
            grants.save_with(&self.path, store)?;
        }
        Ok(named || deleted)
    }

    /// The `Authorization` header for `server`, its token refreshed first when
    /// it has lapsed; `None` when the server holds no grant.
    pub async fn authorization_header(
        &self,
        server: &str,
        now: u64,
    ) -> anyhow::Result<Option<(String, String)>> {
        let store = self.store()?;
        leviath_mcp::OAuthClient::new()
            .authorization_header_with(server, &self.path, now, store)
            .await
    }

    /// What renews `server`'s token on a 401 mid-session, through these same
    /// grants.
    pub fn refresher(&self, server: &str) -> leviath_mcp::StoredTokenRefresher {
        leviath_mcp::StoredTokenRefresher::new(
            server,
            self.path.clone(),
            self.store.clone().unwrap_or_default(),
        )
    }
}

/// A probe that always reports no credential store, for tests and for callers
/// that need the "this machine has no keychain" path without having such a
/// machine.
#[cfg(test)]
pub(crate) fn no_store_available(_service: &str) -> Result<(), String> {
    Err("OS credential store unavailable: no default store".to_string())
}

/// Serialization for the process-wide credential store, shared by every test in
/// this crate that touches it.
///
/// `keyring_core`'s default store is one global. Two modules each holding their
/// *own* mutex would serialize against themselves and race each other, so this
/// lives here - beside the backend it protects - rather than in each test module.
#[cfg(test)]
pub(crate) mod test_store {
    static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the store lock, tolerating poisoning: a test that panicked while
    /// holding it has already failed, and turning that into a cascade of
    /// secondary failures in unrelated tests hides the original.
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Install a fresh in-memory store as the process default, holding the lock
    /// for the caller's lifetime.
    ///
    /// This is what lets the real `leviath_sys::keychain` path run in tests:
    /// no CI runner has an unlocked login keychain, and reaching a developer's
    /// real one would both prompt and write.
    pub(crate) fn with_mock() -> std::sync::MutexGuard<'static, ()> {
        let guard = lock();
        keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
        guard
    }

    /// Whether [`store_for`](super::store_for) answers as a machine with no
    /// credential store. Set only while an [`Unreachable`] guard lives.
    pub(crate) static UNREACHABLE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// A machine with no credential store, for as long as the guard lives.
    ///
    /// What lets a surface that resolves its own backend be driven down its
    /// "keychain asked for and unreachable" path: the real probe cannot be
    /// made to fail without reaching the real keychain. Holds the store lock,
    /// so no other test resolves the keychain meanwhile.
    pub(crate) fn unreachable() -> Unreachable {
        let guard = lock();
        UNREACHABLE.store(true, std::sync::atomic::Ordering::SeqCst);
        Unreachable { _guard: guard }
    }

    /// See [`unreachable`]: clears the flag, then releases the lock.
    pub(crate) struct Unreachable {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Unreachable {
        fn drop(&mut self) {
            UNREACHABLE.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::test_store;

    fn with_mock_store() -> std::sync::MutexGuard<'static, ()> {
        test_store::with_mock()
    }

    /// The adapter over the real `leviath_sys::keychain` path, driven against an
    /// in-memory store installed as the process default - see the module docs in
    /// `leviath-sys` for why reaching a real keychain is not an option in tests.
    #[test]
    fn the_keychain_adapter_round_trips_a_secret() {
        let _guard = with_mock_store();
        let store = KeychainStore::new("dev.leviath.test.adapter");

        let account = leviath_core::provider_account("anthropic");
        assert_eq!(store.get(&account).unwrap(), None);
        store.set(&account, "sk-ant-x").unwrap();
        assert_eq!(store.get(&account).unwrap().as_deref(), Some("sk-ant-x"));
        assert!(store.delete(&account).unwrap());
        assert!(!store.delete(&account).unwrap());
    }

    /// `file` is the default and must not consult the OS at all - the probe is
    /// never even called, so a machine with no keychain is unaffected.
    #[test]
    fn the_file_backend_never_probes() {
        static PROBED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn record(_: &str) -> Result<(), String> {
            PROBED.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        assert!(
            store_for_with(CredentialStoreKind::File, record)
                .unwrap()
                .is_none(),
            "the file backend is `None`, not a store"
        );
        assert!(
            !PROBED.load(std::sync::atomic::Ordering::Relaxed),
            "the file backend must not probe for an OS credential store"
        );
        // ...and the same probe *is* used for the keychain, so the check above
        // is about the file path rather than about `record` never running.
        assert!(store_for_with(CredentialStoreKind::Keychain, record).is_ok());
        assert!(PROBED.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// Asking for the keychain on a machine that has none must say which
    /// setting caused it - otherwise the error looks like a Leviath bug rather
    /// than a configuration choice.
    #[test]
    fn asking_for_an_unavailable_keychain_names_the_setting() {
        // `.err()` rather than `expect_err`, which would need `Debug` on the
        // boxed trait object - and leaves no unreachable `Ok` arm behind.
        let err = store_for_with(CredentialStoreKind::Keychain, no_store_available)
            .err()
            .expect("a failing probe must not yield a store");
        assert!(err.contains(r#"credential_store = "keychain""#), "{err}");
        assert!(err.contains("credential store unavailable"), "{err}");
    }

    /// Grants read and written through a store keep the tokens out of the
    /// file, and forgetting one takes it out of both.
    #[test]
    fn grants_kept_in_a_store_stay_out_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let grants = McpGrants::new(
            dir.path().join("mcp-auth.json"),
            Ok(Some(Box::new(leviath_core::MemoryStore::new()))),
        );
        let mut held = grants.load().unwrap();
        assert!(held.server_names().is_empty());
        held.set(
            "remote",
            leviath_mcp::ServerAuth {
                access_token: "secret-token".to_string(),
                ..Default::default()
            },
        );
        grants.save(&held).unwrap();
        let file = std::fs::read_to_string(grants.path()).unwrap();
        assert!(!file.contains("secret-token"), "{file}");
        assert_eq!(
            grants.load().unwrap().get("remote").unwrap().access_token,
            "secret-token"
        );

        assert!(grants.forget("remote").unwrap());
        assert!(!grants.forget("remote").unwrap(), "nothing left to forget");
        assert!(grants.load().unwrap().get("remote").is_none());
    }

    /// Forgetting a grant held in the store takes the secret out of the store,
    /// not only its name out of the file: a refresh token left there outlives
    /// the login it was for.
    #[test]
    fn forgetting_a_grant_takes_its_secret_out_of_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let grants = McpGrants::new(
            dir.path().join("mcp-auth.json"),
            Ok(Some(Box::new(leviath_core::MemoryStore::new()))),
        );
        let mut held = AuthStore::default();
        for server in ["remote", "other"] {
            held.set(
                server,
                leviath_mcp::ServerAuth {
                    refresh_token: Some("refresh-secret".to_string()),
                    ..Default::default()
                },
            );
        }
        grants.save(&held).unwrap();
        let store = grants.store().unwrap().unwrap();
        let account = leviath_core::mcp_account("remote");
        assert!(store.get(&account).unwrap().is_some());

        assert!(grants.forget("remote").unwrap());
        assert_eq!(store.get(&account).unwrap(), None, "the secret went too");
        assert!(
            store
                .get(&leviath_core::mcp_account("other"))
                .unwrap()
                .is_some(),
            "and only that one"
        );

        // A secret the file no longer names goes too, so one already stranded
        // there has a way out.
        let stray = leviath_core::mcp_account("stray");
        store.set(&stray, "{}").unwrap();
        assert!(grants.forget("stray").unwrap());
        assert_eq!(store.get(&stray).unwrap(), None);
    }

    /// A locked keychain costs the MCP servers that need OAuth, not every
    /// server an agent connects to: the file alone is read instead.
    #[test]
    fn an_unreachable_keychain_leaves_the_file_alone_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let file_alone = McpGrants::or_file(path.clone(), Err("no keychain here".to_string()));
        assert!(file_alone.load().unwrap().server_names().is_empty());

        let keychain = McpGrants::or_file(
            path.clone(),
            Ok(Some(Box::new(leviath_core::MemoryStore::new()))),
        );
        assert!(keychain.store().unwrap().is_some());
        assert!(
            McpGrants::at_or_file(path, CredentialStoreKind::File)
                .store()
                .unwrap()
                .is_none()
        );
    }

    /// A listing reads what it can and says what it could not: the file alone
    /// when the keychain is unreachable, nothing when the file will not load.
    #[test]
    fn a_listing_shows_what_it_can_read_and_says_why_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let mut held = AuthStore::default();
        held.set("remote", leviath_mcp::ServerAuth::default());
        held.save(&path).unwrap();

        let (shown, why) = McpGrants::at(path.clone(), CredentialStoreKind::File).shown();
        assert!(shown.get("remote").is_some());
        assert_eq!(why, None);

        let (shown, why) =
            McpGrants::new(path.clone(), Err("no keychain here".to_string())).shown();
        assert!(shown.get("remote").is_some(), "the file alone is read");
        assert_eq!(why.as_deref(), Some("no keychain here"));

        std::fs::write(&path, "not json").unwrap();
        let (shown, why) = McpGrants::at(path.clone(), CredentialStoreKind::File).shown();
        assert!(shown.server_names().is_empty());
        assert!(why.is_some_and(|why| why.contains("corrupt")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    /// A store that will not delete fails the forget before the file is
    /// touched, so the server is still named there for the next attempt.
    #[test]
    fn a_store_that_will_not_delete_fails_the_forget() {
        struct KeepsEverything(leviath_core::MemoryStore);
        impl CredentialStore for KeepsEverything {
            fn get(&self, account: &str) -> Result<Option<String>, String> {
                self.0.get(account)
            }
            fn set(&self, account: &str, secret: &str) -> Result<(), String> {
                self.0.set(account, secret)
            }
            fn delete(&self, _: &str) -> Result<bool, String> {
                Err("the keychain is locked".to_string())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let grants = McpGrants::new(
            dir.path().join("mcp-auth.json"),
            Ok(Some(Box::new(KeepsEverything(
                leviath_core::MemoryStore::new(),
            )))),
        );
        let mut held = AuthStore::default();
        held.set("remote", leviath_mcp::ServerAuth::default());
        grants.save(&held).unwrap();

        let err = grants.forget("remote").unwrap_err();
        assert!(err.to_string().contains("the keychain is locked"), "{err}");
        assert!(grants.load().unwrap().get("remote").is_some());
    }

    /// A keychain that cannot be reached is every read's and write's answer,
    /// and never a quiet fall back to the file.
    #[tokio::test]
    async fn an_unreachable_keychain_is_the_answer_to_every_use() {
        let dir = tempfile::tempdir().unwrap();
        let grants = McpGrants::new(
            dir.path().join("mcp-auth.json"),
            Err("no keychain here".to_string()),
        );
        let err = grants.load().unwrap_err();
        assert!(err.to_string().contains("no keychain"), "{err}");
        assert!(grants.save(&AuthStore::default()).is_err());
        assert!(grants.forget("remote").is_err());
        assert!(grants.authorization_header("remote", 0).await.is_err());
        assert!(!grants.path().exists(), "nothing was written");
        // A refresher is only made for a grant that was read; this one has
        // only the file to read.
        let _ = grants.refresher("remote");
    }

    /// The bearer comes from the stored grant, and a server with none has
    /// none.
    #[tokio::test]
    async fn the_bearer_is_the_stored_token() {
        let dir = tempfile::tempdir().unwrap();
        let grants = McpGrants::at(dir.path().join("mcp-auth.json"), CredentialStoreKind::File);
        assert_eq!(
            grants.authorization_header("remote", 0).await.unwrap(),
            None
        );
        let mut held = AuthStore::default();
        held.set(
            "remote",
            leviath_mcp::ServerAuth {
                access_token: "live".to_string(),
                expires_at: u64::MAX,
                ..Default::default()
            },
        );
        grants.save(&held).unwrap();
        assert_eq!(
            grants.authorization_header("remote", 0).await.unwrap(),
            Some(("Authorization".to_string(), "Bearer live".to_string()))
        );
    }

    #[test]
    fn the_keychain_backend_resolves_to_a_store() {
        let _guard = with_mock_store();
        assert!(
            store_for(CredentialStoreKind::Keychain).unwrap().is_some(),
            "with a store available the keychain backend resolves"
        );
    }
}
