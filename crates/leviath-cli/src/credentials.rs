//! Choosing and using the configured credential backend.
//!
//! [`leviath_core::credentials`] defines the vocabulary and the
//! [`CredentialStore`] trait; `leviath_sys::keychain` owns the OS binding and
//! its no-store fallback. This module is the seam between them: it turns a
//! `[security] credential_store` setting into something the config loader and
//! the `lev auth` command can call.

use leviath_core::{CredentialStore, CredentialStoreKind};

/// The provider API keys Leviath knows how to move into a credential store.
///
/// Fixed, because the OS stores offer no portable "list everything under this
/// service" operation - the accounts to look for have to come from somewhere,
/// and for providers that is this list.
pub const PROVIDER_KEYS: &[&str] = &["anthropic", "openai", "google", "openrouter"];

/// A [`CredentialStore`] backed by the OS credential store.
///
/// A thin adapter over `leviath_sys::keychain`: the platform work, the feature
/// gate, and the "no store available" fallback all live there, so this is only
/// the trait impl that lets the rest of the CLI stay generic over the backend.
pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    /// A store filing credentials under `service`.
    ///
    /// Availability is [`store_for`]'s job, not this one's: it probes once and
    /// reports a missing keychain there, rather than letting every individual
    /// key read fail separately with an error that reads like a missing key.
    pub fn new(service: &str) -> Self {
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
    store_for_with(kind, leviath_sys::keychain::probe)
}

/// The resolved backend: `Ok(None)` for the file store, `Ok(Some(_))` for a
/// working keychain, `Err` for a keychain that was asked for but is unreachable.
pub type Resolved = Result<Option<Box<dyn CredentialStore>>, String>;

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

    #[test]
    fn the_keychain_backend_resolves_to_a_store() {
        let _guard = with_mock_store();
        assert!(
            store_for(CredentialStoreKind::Keychain).unwrap().is_some(),
            "with a store available the keychain backend resolves"
        );
    }

    /// The provider list is what `lev auth migrate` and `lev auth status`
    /// enumerate, so a provider missing from it is a secret that silently never
    /// migrates.
    #[test]
    fn every_provider_with_a_config_key_is_listed() {
        for p in ["anthropic", "openai", "google", "openrouter"] {
            assert!(PROVIDER_KEYS.contains(&p), "{p} must be migratable");
        }
    }
}
