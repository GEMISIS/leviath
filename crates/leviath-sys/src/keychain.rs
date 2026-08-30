//! The OS credential store: macOS Keychain, Windows Credential Manager, and
//! the Secret Service on other Unixes.
//!
//! This is the only place in the workspace that talks to a platform credential
//! store. Callers work in terms of an *account* string and get plain
//! `Result<_, String>` back; which OS facility answers, and whether one exists
//! at all, is entirely this module's problem.
//!
//! # Platforms without a credential store
//!
//! There are two separate ways this can be unavailable, and both are ordinary
//! rather than exceptional:
//!
//! 1. **Not built in.** The `keychain` feature is on by default but can be
//!    turned off - for a target the `keyring` crate does not support, or for a
//!    minimal build that should not link platform credential libraries at all.
//!    With it off, [`is_supported`] is `false` and every operation returns a
//!    plain "not supported in this build" error.
//! 2. **Built in, but nothing to talk to.** A headless Linux box with no Secret
//!    Service running, a locked keychain, a container, an SSH session with no
//!    session keyring. The code is present, the platform simply has no store.
//!
//! Neither is treated as a crash or a panic, and neither is silently swallowed.
//! Leviath's default credential backend is its own `0600` files precisely
//! because these situations are common; the keychain is opt-in, so an
//! unavailable one costs a clear error at startup and nothing else.
//!
//! # Why the split into `raw_*` and `interpret_*`
//!
//! The functions that touch the OS are deliberately branch-free delegation, and
//! every decision about what a result *means* lives in a pure function beside
//! them. That is what makes this module testable: no CI runner has an unlocked
//! login keychain, and a test that reached a developer's real one would raise an
//! OS authorization prompt and hang. Tests install an in-memory store as the
//! process default and drive this same code, unchanged, all the way down.

/// Whether this build can talk to an OS credential store at all.
///
/// `false` when the `keychain` feature is off. Note that `true` does not promise
/// a *working* store - see the module docs; use [`probe`] for that.
pub const fn is_supported() -> bool {
    cfg!(feature = "keychain")
}

/// Check that a usable credential store exists, without storing anything.
///
/// Worth calling once at startup rather than discovering the problem at the
/// first secret read: on a headless box every key lookup would otherwise fail
/// one at a time with an error that reads like a missing key rather than a
/// missing keychain.
pub fn probe(service: &str) -> Result<(), String> {
    imp::probe(service)
}

/// The secret stored for `account`, or `None` when there is none.
///
/// A *missing* entry is `Ok(None)`; an unreachable or locked store is `Err`.
/// The distinction matters: collapsing a locked keychain into `None` would
/// surface to the user as "no API key configured" and send them to re-run
/// `lev setup` instead of unlocking their keychain.
pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
    imp::get(service, account)
}

/// Store `secret` under `account`, replacing anything already there.
pub fn set(service: &str, account: &str, secret: &str) -> Result<(), String> {
    imp::set(service, account, secret)
}

/// Remove `account`, reporting whether anything was actually removed.
///
/// Deleting something that was not there is `Ok(false)`, not an error, so a
/// caller can clean up unconditionally.
pub fn delete(service: &str, account: &str) -> Result<bool, String> {
    imp::delete(service, account)
}

/// The real implementation, when the `keychain` feature is on.
///
/// Mirrors the crate's `platform` module idiom: exactly one of the two `imp`
/// modules is compiled, so the unused one is never emitted and never counts as
/// a coverage gap.
#[cfg(feature = "keychain")]
mod imp {
    /// Bind the process to the OS-native store and report whether that worked.
    ///
    /// `keyring::Entry::new` installs the platform store on first use, so
    /// constructing one throwaway entry is both how the installation is
    /// triggered and how its failure is reported. Constructing an entry does not
    /// create a credential, so this leaves nothing behind in the user's
    /// keychain.
    ///
    /// If a store is *already* installed, that one is kept: `keyring::Entry::new`
    /// would otherwise replace it with the platform store, which is both wrong
    /// for an embedder that chose its own and actively dangerous under test.
    pub(super) fn probe(service: &str) -> Result<(), String> {
        // Never replace a store that is already installed. `keyring::Entry::new`
        // installs the *platform* store unconditionally on first use, so calling
        // it blindly would clobber a store the embedding process chose - and,
        // in tests, silently redirect every subsequent operation at the
        // developer's real login keychain, where it would both prompt and write.
        if keyring_core::get_default_store().is_some() {
            return Ok(());
        }
        describe(keyring::Entry::new(service, PROBE_ACCOUNT).map(drop))
    }

    /// The account used only to check that a store exists. Never written to.
    const PROBE_ACCOUNT: &str = "__leviath_probe__";

    pub(super) fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        interpret_get(raw_get(service, account))
    }

    pub(super) fn set(service: &str, account: &str, secret: &str) -> Result<(), String> {
        describe(raw_set(service, account, secret))
    }

    pub(super) fn delete(service: &str, account: &str) -> Result<bool, String> {
        interpret_delete(raw_delete(service, account))
    }

    // ─── The OS-touching leaves: delegation only, no decisions ───────────────

    fn raw_get(service: &str, account: &str) -> Result<String, keyring::Error> {
        keyring_core::Entry::new(service, account).and_then(|e| e.get_password())
    }

    fn raw_set(service: &str, account: &str, secret: &str) -> Result<(), keyring::Error> {
        keyring_core::Entry::new(service, account).and_then(|e| e.set_password(secret))
    }

    fn raw_delete(service: &str, account: &str) -> Result<(), keyring::Error> {
        keyring_core::Entry::new(service, account).and_then(|e| e.delete_credential())
    }

    // ─── The decisions: pure, and directly tested ───────────────────────────

    /// "No such entry" is an answer; everything else is a failure.
    fn interpret_get(result: Result<String, keyring::Error>) -> Result<Option<String>, String> {
        match result {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(unavailable(e)),
        }
    }

    /// Deleting something absent is a no-op, not a failure.
    fn interpret_delete(result: Result<(), keyring::Error>) -> Result<bool, String> {
        match result {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(unavailable(e)),
        }
    }

    fn describe(result: Result<(), keyring::Error>) -> Result<(), String> {
        result.map_err(unavailable)
    }

    /// Render a store failure with the one piece of context users need.
    ///
    /// `keyring`'s own message describes the platform call that failed; on a
    /// headless Linux box the answer is nearly always "no Secret Service is
    /// running", and the fix is to go back to the file backend. Naming that here
    /// saves a support round trip.
    fn unavailable(e: keyring::Error) -> String {
        format!(
            "OS credential store unavailable: {e}. Set `[security] credential_store = \"file\"` \
             in ~/.leviath/config.toml to keep secrets in Leviath's own 0600 files instead."
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// `keyring_core`'s default store is process-wide, so a test that
        /// installs one races every test that reads it.
        static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// Install a fresh in-memory store as the process default, holding the
        /// lock for the caller's lifetime. This is what lets the `raw_*`
        /// functions - the real `keyring_core::Entry` path, unchanged - run in
        /// CI on every platform.
        fn with_mock_store() -> std::sync::MutexGuard<'static, ()> {
            let guard = STORE.lock().expect("store lock");
            keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
            guard
        }

        const SVC: &str = "dev.leviath.test";

        #[test]
        fn a_secret_survives_a_set_get_delete_round_trip() {
            let _guard = with_mock_store();

            assert_eq!(get(SVC, "provider/anthropic").unwrap(), None);
            set(SVC, "provider/anthropic", "sk-ant-secret").unwrap();
            assert_eq!(
                get(SVC, "provider/anthropic").unwrap().as_deref(),
                Some("sk-ant-secret")
            );

            // A second set replaces rather than duplicating.
            set(SVC, "provider/anthropic", "sk-ant-rotated").unwrap();
            assert_eq!(
                get(SVC, "provider/anthropic").unwrap().as_deref(),
                Some("sk-ant-rotated")
            );

            assert!(delete(SVC, "provider/anthropic").unwrap(), "it existed");
            assert_eq!(get(SVC, "provider/anthropic").unwrap(), None, "and is gone");
            assert!(
                !delete(SVC, "provider/anthropic").unwrap(),
                "deleting again reports nothing was removed"
            );
        }

        /// The public shims at the crate root - the path every caller actually
        /// takes - driven against the mock store under the lock.
        #[test]
        fn the_public_api_delegates_to_this_implementation() {
            let _guard = with_mock_store();
            const SVC: &str = "dev.leviath.test.shim";

            crate::keychain::probe(SVC).expect("a mock store is available");
            assert_eq!(crate::keychain::get(SVC, "provider/none").unwrap(), None);
            crate::keychain::set(SVC, "provider/none", "v").unwrap();
            assert_eq!(
                crate::keychain::get(SVC, "provider/none")
                    .unwrap()
                    .as_deref(),
                Some("v")
            );
            assert!(crate::keychain::delete(SVC, "provider/none").unwrap());
        }

        #[test]
        fn accounts_are_independent() {
            let _guard = with_mock_store();
            set(SVC, "provider/openai", "one").unwrap();
            set(SVC, "mcp/github", "two").unwrap();
            assert_eq!(get(SVC, "provider/openai").unwrap().as_deref(), Some("one"));
            assert_eq!(get(SVC, "mcp/github").unwrap().as_deref(), Some("two"));
        }

        /// A locked or absent store is an error, not a silent `None` - the
        /// distinction the module docs turn on.
        #[test]
        fn a_missing_entry_is_none_but_a_broken_store_is_an_error() {
            assert_eq!(interpret_get(Ok("s".into())).unwrap().as_deref(), Some("s"));
            assert_eq!(interpret_get(Err(keyring::Error::NoEntry)).unwrap(), None);
            assert!(interpret_get(Err(keyring::Error::NoDefaultStore)).is_err());

            assert!(interpret_delete(Ok(())).unwrap());
            assert!(!interpret_delete(Err(keyring::Error::NoEntry)).unwrap());
            assert!(interpret_delete(Err(keyring::Error::NoDefaultStore)).is_err());

            assert!(describe(Ok(())).is_ok());
            assert!(describe(Err(keyring::Error::NoDefaultStore)).is_err());
        }

        /// Every failure must name the way out. A user whose keychain is
        /// unavailable needs to know the file backend is one config line away,
        /// not just that a platform call failed.
        #[test]
        fn failures_name_the_file_fallback() {
            let msg = unavailable(keyring::Error::NoDefaultStore);
            assert!(msg.contains("OS credential store unavailable"), "{msg}");
            assert!(msg.contains("credential_store = \"file\""), "{msg}");
        }

        /// With no store installed at all, every operation reports rather than
        /// panicking - the headless-Linux case.
        #[test]
        fn every_operation_reports_when_there_is_no_store() {
            let _guard = STORE.lock().expect("store lock");
            keyring_core::unset_default_store();

            assert!(get(SVC, "provider/anthropic").is_err());
            assert!(set(SVC, "provider/anthropic", "x").is_err());
            assert!(delete(SVC, "provider/anthropic").is_err());
        }

        /// The real platform probe. Either outcome is legitimate - a developer
        /// machine has a credential store and a CI runner does not - so what is
        /// asserted is that it answers rather than panicking or prompting. It
        /// contributes no branch of its own, which is why one call covers it on
        /// every platform.
        #[test]
        fn the_platform_probe_answers_on_any_platform() {
            let _guard = STORE.lock().expect("store lock");
            // Clear the default store so the probe takes its *other* branch --
            // the one that actually asks the platform. Constructing an `Entry`
            // creates a handle, not a credential, so this reads and writes
            // nothing in a developer's real keychain.
            keyring_core::unset_default_store();
            let _ = probe(SVC);
            // The probe may have installed the real platform store; put the mock
            // back before releasing the lock, so nothing that runs next can
            // reach the OS.
            keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
        }

        /// The short-circuit itself: with a store already installed, the probe
        /// must accept it rather than replacing it with the platform store.
        #[test]
        fn the_probe_keeps_a_store_that_is_already_installed() {
            let _guard = with_mock_store();
            probe(SVC).expect("an installed store is a usable store");
            // Still the mock: a real platform store would have replaced it, and
            // every later operation would hit the OS.
            set(SVC, "provider/probe-check", "v").unwrap();
            assert_eq!(
                get(SVC, "provider/probe-check").unwrap().as_deref(),
                Some("v")
            );
        }
    }
}

/// The stub implementation, when the `keychain` feature is off.
///
/// Every operation reports plainly that this build has no credential store,
/// rather than pretending an empty one. A caller that asked for the keychain
/// backend on a build without it has a configuration problem, and silently
/// answering "no secrets found" would look identical to a wiped keychain.
#[cfg(not(feature = "keychain"))]
mod imp {
    const UNSUPPORTED: &str = "this build of Leviath has no OS credential store support \
         (the `keychain` feature is disabled). Set `[security] credential_store = \"file\"` \
         in ~/.leviath/config.toml to keep secrets in Leviath's own 0600 files instead.";

    pub(super) fn probe(_service: &str) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    pub(super) fn get(_service: &str, _account: &str) -> Result<Option<String>, String> {
        Err(UNSUPPORTED.to_string())
    }

    pub(super) fn set(_service: &str, _account: &str, _secret: &str) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    pub(super) fn delete(_service: &str, _account: &str) -> Result<bool, String> {
        Err(UNSUPPORTED.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_supported` reports what was actually compiled in - a build with the
    /// feature off must say so rather than claiming a store it cannot reach.
    ///
    /// The other public shims are exercised from `imp`'s tests, which hold the
    /// process-wide store lock. Calling them from here would race those tests,
    /// and on a machine that *has* a keychain an unguarded call reaches it.
    #[test]
    fn support_matches_the_feature() {
        assert_eq!(is_supported(), cfg!(feature = "keychain"));
    }
}
