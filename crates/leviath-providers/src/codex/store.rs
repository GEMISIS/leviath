//! On-disk store for provider OAuth grants.
//!
//! Deliberately mirrors `leviath-mcp/src/auth/store.rs`, which solved the same
//! problem for MCP servers: the file-versus-keychain split, the on-disk index
//! of keychain-held names (the OS stores have no portable enumerate), the
//! `write_private` 0600 write, and the policy that an unreadable grant is
//! dropped rather than failing the whole load.
//!
//! It is a separate type rather than a second consumer of `AuthStore` because
//! `ServerAuth` carries an RFC 8707 `resource` and a dynamic-registration
//! client id, neither of which a provider grant has, and because an MCP server
//! named `codex` would otherwise collide with the provider in one namespace.
//! Lifting a generic `GrantStore<T>` out of the two is the purer answer and a
//! much larger change than this feature earns; this comment is here so the
//! duplication is a decision rather than an accident.
//!
//! Kept out of `config.toml` for the reason that file gives: the config is
//! round-tripped and rewritten by the CLI, and refresh tokens have no business
//! passing through that path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One provider's OAuth grant.
///
/// `Debug` is hand-written below so the tokens cannot be printed.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderGrant {
    /// Current bearer token. A JWT, so its expiry is readable without asking.
    pub access_token: String,
    /// Used once to mint the next pair. **Rotates**: the server invalidates
    /// this value as soon as it issues a replacement, and presenting a spent
    /// one is terminal for the whole grant.
    pub refresh_token: String,
    /// The id token, kept for the account claims rather than for auth.
    #[serde(default)]
    pub id_token: String,
    /// The workspace this grant acts in, sent as `ChatGPT-Account-Id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The plan tier at login, for display and for gating the model catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    /// The signed-in address, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Hand-written so the access and refresh tokens can never be printed.
///
/// This struct is carried through error paths that format their context, and
/// one `{:?}` would put a live subscription credential in the logs. The
/// account and plan are the useful part of a debug line anyway.
impl std::fmt::Debug for ProviderGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderGrant")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("id_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("plan_type", &self.plan_type)
            .field("email", &self.email)
            .finish()
    }
}

impl ProviderGrant {
    /// Would the access token be expired at `now` (Unix seconds), within a
    /// refresh margin?
    ///
    /// The margin refreshes a token that is about to lapse rather than letting
    /// the next request fail and retry. A token whose expiry cannot be read
    /// reads as *not* expired: there is nothing to schedule against, so the
    /// reactive 401 path is the only honest answer and pretending otherwise
    /// would refresh on every single call.
    pub fn is_expired_at(&self, now: u64) -> bool {
        match super::claims::expiry(&self.access_token) {
            Some(exp) => exp <= now.saturating_add(REFRESH_MARGIN_SECS),
            None => false,
        }
    }

    /// The account facts carried in the id token.
    pub fn claims(&self) -> super::claims::CodexClaims {
        super::claims::parse(&self.id_token)
    }
}

/// How far ahead of expiry a token is refreshed.
///
/// Two minutes rather than the sixty seconds the MCP store uses: a Leviath
/// request can carry a whole assembled context window, and re-uploading one
/// because the token lapsed mid-flight is far more expensive than refreshing
/// slightly early.
pub const REFRESH_MARGIN_SECS: u64 = 120;

/// The keychain account name a provider grant is stored under.
///
/// Deliberately not `leviath_core::provider_account`, which is `provider/<name>`
/// and is already the namespace API keys use. They would not collide today, but
/// the cost of guaranteeing that is one function.
pub fn grant_account(provider: &str) -> String {
    format!("provider-oauth/{provider}")
}

/// A [`leviath_core::CredentialStore`] backed by the OS credential store.
///
/// A twin of the CLI's `KeychainStore`, and deliberately so: the registry is
/// built in the runtime, which has no way to reach the CLI's, and a grant that
/// only the CLI could read would leave `[security] credential_store =
/// "keychain"` silently signing the daemon out. Both are the same handful of
/// lines over `leviath_sys::keychain`, which owns the platform work.
struct KeychainStore;

impl leviath_core::CredentialStore for KeychainStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        leviath_sys::keychain::get(leviath_core::credentials::SERVICE, account)
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        leviath_sys::keychain::set(leviath_core::credentials::SERVICE, account, secret)
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        leviath_sys::keychain::delete(leviath_core::credentials::SERVICE, account)
    }
}

/// The credential store for `kind`, or `None` when grants belong in the file.
///
/// `None` is the ordinary answer: `file` is the default backend.
pub fn store_for(
    kind: leviath_core::CredentialStoreKind,
) -> Option<Arc<dyn leviath_core::CredentialStore>> {
    match kind {
        leviath_core::CredentialStoreKind::File => None,
        leviath_core::CredentialStoreKind::Keychain => Some(Arc::new(KeychainStore)),
    }
}

/// Every provider grant on this machine.
///
/// With `[security] credential_store = "keychain"` the grants live in the OS
/// store and this file keeps only their names, for the same reason the MCP
/// store does: the OS stores offer no portable "list everything" operation, so
/// without the names `lev auth status` could not report what is signed in. A
/// provider name is not a secret; the tokens are, and those are what move.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderAuthStore {
    #[serde(default)]
    providers: HashMap<String, ProviderGrant>,

    /// Providers whose grant is held in the OS credential store, not here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keychain_providers: Vec<String>,
}

impl ProviderAuthStore {
    /// Load from `path`, or an empty store if it does not exist.
    ///
    /// A missing file is normal (nobody has signed in yet); a present but
    /// corrupt one is an error, because silently starting empty would drop a
    /// working grant and send the user back through a browser for no reason.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::load_with(path, None)
    }

    /// [`load`](Self::load), pulling keychain-held grants out of `store`.
    ///
    /// A grant the credential store cannot return is dropped with a warning
    /// rather than failing the load: the user is then signed out of that one
    /// provider, which `lev auth login` fixes, whereas refusing to load would
    /// take every other provider down with it.
    pub fn load_with(
        path: &Path,
        store: Option<&dyn leviath_core::CredentialStore>,
    ) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read the provider auth store: {e}"))?;
        let mut this: Self = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("the provider auth store is corrupt: {e}"))?;

        if let Some(store) = store {
            for name in this.keychain_providers.clone() {
                let Ok(Some(json)) = store.get(&grant_account(&name)) else {
                    tracing::warn!(
                        "no stored credential for provider '{name}'; \
                         run `lev auth login {name}` to sign in again"
                    );
                    continue;
                };
                match serde_json::from_str::<ProviderGrant>(&json) {
                    Ok(grant) => {
                        this.providers.insert(name.clone(), grant);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "stored credential for provider '{name}' is unreadable ({e}); \
                             run `lev auth login {name}` to sign in again"
                        );
                    }
                }
            }
        }
        Ok(this)
    }

    /// Write to `path` with owner-only permissions.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.save_with(path, None)
    }

    /// [`save`](Self::save), putting the grants in `store` instead of the file.
    ///
    /// A store that refuses the write fails the whole save. Falling back to the
    /// file would leave a user who asked for the keychain with a plaintext
    /// refresh token on disk and nothing saying so.
    pub fn save_with(
        &self,
        path: &Path,
        store: Option<&dyn leviath_core::CredentialStore>,
    ) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            leviath_sys::create_private_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", parent.display()))?;
        }

        let to_write = match store {
            None => self.clone(),
            Some(store) => {
                let mut names: Vec<String> = Vec::new();
                for (name, grant) in &self.providers {
                    let json =
                        serde_json::to_string(grant).expect("ProviderGrant is always serializable");
                    store.set(&grant_account(name), &json).map_err(|e| {
                        anyhow::anyhow!("failed to store the grant for '{name}': {e}")
                    })?;
                    names.push(name.clone());
                }
                names.sort();
                Self {
                    providers: HashMap::new(),
                    keychain_providers: names,
                }
            }
        };

        let content = serde_json::to_string_pretty(&to_write)
            .expect("ProviderAuthStore is always serializable");
        // `write_private`, not a write followed by a chmod: the two-step
        // version leaves refresh tokens at the umask default in between.
        leviath_sys::write_private(path, content.as_bytes())
            .map_err(|e| anyhow::anyhow!("failed to write the provider auth store: {e}"))?;
        Ok(())
    }

    /// The default store path, honouring `LEVIATH_HOME` like every other
    /// home-relative path in this workspace.
    ///
    /// `None` when no home can be resolved, so the caller reports a missing
    /// home rather than this silently picking a surprising fallback.
    pub fn default_path() -> Option<PathBuf> {
        leviath_core::data_dir().map(|dir| dir.join("provider-auth.json"))
    }

    /// Look up a provider's grant.
    pub fn get(&self, provider: &str) -> Option<&ProviderGrant> {
        self.providers.get(provider)
    }

    /// Insert or replace a provider's grant.
    pub fn set(&mut self, provider: &str, grant: ProviderGrant) {
        self.providers.insert(provider.to_string(), grant);
    }

    /// Remove a grant, returning whether anything was removed.
    ///
    /// Also drops it from the keychain index, so a later `save_with` does not
    /// resurrect a name whose grant is gone.
    pub fn remove(&mut self, provider: &str) -> bool {
        let had_index = self.keychain_providers.iter().any(|n| n == provider);
        self.keychain_providers.retain(|n| n != provider);
        self.providers.remove(provider).is_some() || had_index
    }

    /// Every provider and its grant, sorted by name.
    ///
    /// Paired rather than a name list the caller looks each one up in: the
    /// lookup could not fail, so writing it costs a branch nothing can take.
    pub fn entries(&self) -> Vec<(String, ProviderGrant)> {
        let mut entries: Vec<(String, ProviderGrant)> = self
            .providers
            .iter()
            .map(|(name, grant)| (name.clone(), grant.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Every provider with a grant, sorted, for `lev auth status`.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use leviath_core::CredentialStore as _;

    fn grant_expiring_at(exp: u64) -> ProviderGrant {
        ProviderGrant {
            access_token: format!(
                "aGVhZGVy.{}.c2ln",
                URL_SAFE_NO_PAD.encode(serde_json::json!({ "exp": exp }).to_string())
            ),
            refresh_token: "rt-secret".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_token_is_expired_once_it_is_inside_the_refresh_margin() {
        let grant = grant_expiring_at(1_000);
        assert!(!grant.is_expired_at(1_000 - REFRESH_MARGIN_SECS - 1));
        assert!(grant.is_expired_at(1_000 - REFRESH_MARGIN_SECS));
        assert!(grant.is_expired_at(1_000));
    }

    #[test]
    fn an_unreadable_expiry_never_reads_as_expired() {
        // Otherwise every call would refresh, spending a rotation each time.
        let grant = ProviderGrant {
            access_token: "not-a-jwt".to_string(),
            ..Default::default()
        };
        assert!(!grant.is_expired_at(u64::MAX));
    }

    #[test]
    fn the_expiry_check_cannot_overflow() {
        // `now + margin` at the top of the range must saturate, not wrap;
        // wrapping would report a live token as expired.
        assert!(grant_expiring_at(u64::MAX).is_expired_at(u64::MAX));
    }

    #[test]
    fn debug_never_prints_a_token() {
        let grant = ProviderGrant {
            access_token: "at-super-secret".to_string(),
            refresh_token: "rt-super-secret".to_string(),
            id_token: "id-super-secret".to_string(),
            account_id: Some("acct-1".to_string()),
            plan_type: Some("plus".to_string()),
            email: Some("a@b.c".to_string()),
        };
        let rendered = format!("{grant:?}");
        assert!(!rendered.contains("super-secret"), "leaked: {rendered}");
        assert!(
            rendered.contains("acct-1"),
            "lost the useful part: {rendered}"
        );
        assert!(rendered.contains("plus"));
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProviderAuthStore::load(&dir.path().join("nope.json")).unwrap();
        assert!(store.names().is_empty());
    }

    #[test]
    fn a_corrupt_file_is_an_error_rather_than_a_silent_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider-auth.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = ProviderAuthStore::load(&path).unwrap_err().to_string();
        assert!(err.contains("corrupt"), "got: {err}");
    }

    #[test]
    fn an_unreadable_file_is_an_error() {
        // A directory where a file is expected: `read_to_string` fails with an
        // io error rather than a parse error, which is the other arm.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-directory");
        std::fs::create_dir(&path).unwrap();
        let err = ProviderAuthStore::load(&path).unwrap_err().to_string();
        assert!(err.contains("failed to read"), "got: {err}");
    }

    #[test]
    fn a_saved_grant_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("provider-auth.json");
        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        store.save(&path).unwrap();

        let loaded = ProviderAuthStore::load(&path).unwrap();
        assert_eq!(loaded.names(), vec!["codex".to_string()]);
        assert_eq!(loaded.get("codex").unwrap().refresh_token, "rt-secret");
        assert!(loaded.get("nothing").is_none());
    }

    #[test]
    fn a_directory_that_cannot_be_made_fails_the_save() {
        // The parent is a file, so there is nowhere to put the store.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("in-the-way");
        std::fs::write(&blocker, b"").unwrap();
        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        let err = store
            .save(&blocker.join("provider-auth.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to create"), "got: {err}");
    }

    #[test]
    fn a_path_with_no_parent_still_reports_the_write_failure() {
        // The root has no parent, so the directory step is skipped and the
        // write is what fails. Both arms in one path.
        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        let err = store.save(Path::new("/")).unwrap_err().to_string();
        assert!(err.contains("failed to write"), "got: {err}");
    }

    #[test]
    fn entries_come_back_paired_and_sorted() {
        let mut store = ProviderAuthStore::default();
        store.set("zeta", grant_expiring_at(1));
        store.set("alpha", grant_expiring_at(2));
        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "alpha");
        assert_eq!(entries[1].0, "zeta");
        assert_eq!(entries[0].1.refresh_token, "rt-secret");
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let mut store = ProviderAuthStore::default();
        store.set("codex", ProviderGrant::default());
        assert!(store.remove("codex"));
        assert!(!store.remove("codex"));
        assert!(store.names().is_empty());
    }

    #[test]
    fn the_keychain_backend_keeps_names_on_disk_and_tokens_in_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider-auth.json");
        let keychain = leviath_core::MemoryStore::default();

        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        store.save_with(&path, Some(&keychain)).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("rt-secret"),
            "token hit the disk: {on_disk}"
        );
        assert!(on_disk.contains("codex"), "lost the name index: {on_disk}");

        let loaded = ProviderAuthStore::load_with(&path, Some(&keychain)).unwrap();
        assert_eq!(loaded.get("codex").unwrap().refresh_token, "rt-secret");
    }

    #[test]
    fn a_keychain_grant_that_cannot_be_read_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider-auth.json");
        let keychain = leviath_core::MemoryStore::default();

        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        store.set("other", grant_expiring_at(42));
        store.save_with(&path, Some(&keychain)).unwrap();

        // One grant vanishes from the OS store, one becomes unparseable.
        keychain.delete(&grant_account("codex")).unwrap();
        keychain.set(&grant_account("other"), "{ not json").unwrap();

        let loaded = ProviderAuthStore::load_with(&path, Some(&keychain)).unwrap();
        assert!(loaded.get("codex").is_none());
        assert!(loaded.get("other").is_none());
    }

    #[test]
    fn a_store_that_refuses_the_write_fails_the_save() {
        // Never a silent downgrade to a plaintext file.
        struct Refusing;
        impl leviath_core::CredentialStore for Refusing {
            fn get(&self, _: &str) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Err("locked".to_string())
            }
            fn delete(&self, _: &str) -> Result<bool, String> {
                Ok(false)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider-auth.json");
        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        let err = store
            .save_with(&path, Some(&Refusing))
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to store the grant"), "got: {err}");
        assert!(!path.exists(), "a refused save must not write the file");

        // The other two are part of the same contract: a store that refuses a
        // write still answers, so a load through it reports nothing rather
        // than failing.
        assert_eq!(Refusing.get("anything").expect("readable"), None);
        assert!(!Refusing.delete("anything").expect("deletable"));
    }

    #[test]
    fn removing_also_clears_the_keychain_index() {
        // Otherwise a later save_with would resurrect a name whose grant is
        // gone, and `lev auth status` would report a phantom login.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider-auth.json");
        let keychain = leviath_core::MemoryStore::default();
        let mut store = ProviderAuthStore::default();
        store.set("codex", grant_expiring_at(42));
        store.save_with(&path, Some(&keychain)).unwrap();

        let mut loaded = ProviderAuthStore::load(&path).unwrap();
        assert!(
            loaded.remove("codex"),
            "the name index alone counts as present"
        );
        loaded.save(&path).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("codex"), "name survived removal: {after}");
    }

    #[test]
    fn the_default_path_sits_beside_the_mcp_store() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let path = ProviderAuthStore::default_path().expect("home is set");
            let shown = path.display().to_string();
            assert!(path.ends_with("provider-auth.json"), "got {shown}");
        });
    }

    /// Take the store lock, tolerating poisoning: a test that panicked while
    /// holding it has already failed, and cascading into unrelated ones hides
    /// the original.
    fn with_mock_keychain() -> std::sync::MutexGuard<'static, ()> {
        static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
        guard
    }

    /// The adapter over the real `leviath_sys::keychain` path. No CI runner has
    /// an unlocked login keychain, and reaching a developer's real one would
    /// raise an OS prompt and hang, so an in-memory store stands in as the
    /// process default.
    #[test]
    fn the_keychain_adapter_round_trips_a_grant() {
        let _guard = with_mock_keychain();
        let store = KeychainStore;
        let account = grant_account("codex");

        assert_eq!(store.get(&account).expect("a readable store"), None);
        store.set(&account, "{}").expect("a writable store");
        assert_eq!(
            store.get(&account).expect("a readable store").as_deref(),
            Some("{}")
        );
        assert!(store.delete(&account).expect("a deletable store"));
        assert!(
            !store
                .delete(&account)
                .expect("deleting twice is not an error")
        );
    }

    /// `file` is the default and must reach no OS store at all.
    #[test]
    fn only_the_keychain_backend_resolves_to_a_store() {
        assert!(store_for(leviath_core::CredentialStoreKind::File).is_none());
        assert!(store_for(leviath_core::CredentialStoreKind::Keychain).is_some());
    }

    #[test]
    fn the_grant_account_namespace_is_distinct_from_the_api_key_one() {
        assert_eq!(grant_account("codex"), "provider-oauth/codex");
        assert_ne!(
            grant_account("codex"),
            leviath_core::provider_account("codex")
        );
    }

    #[test]
    fn claims_come_from_the_id_token() {
        let grant = ProviderGrant {
            id_token: format!(
                "aGVhZGVy.{}.c2ln",
                URL_SAFE_NO_PAD.encode(
                    serde_json::json!({
                        "https://api.openai.com/auth": { "chatgpt_plan_type": "plus" },
                    })
                    .to_string()
                )
            ),
            ..Default::default()
        };
        assert_eq!(grant.claims().plan_type.as_deref(), Some("plus"));
    }
}
