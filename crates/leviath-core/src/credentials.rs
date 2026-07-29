//! Where Leviath's long-lived secrets live.
//!
//! Two backends, chosen at runtime by `[security] credential_store`:
//!
//! - **`file`** (the default) - provider API keys sit in `~/.leviath/config.toml`
//!   and MCP OAuth tokens in `~/.leviath/mcp-auth.json`, both written `0600` by
//!   `leviath_sys::write_private` so they are never briefly world-readable. This
//!   is what Claude Code and Codex do, and it is a reasonable default: it works
//!   headless, in containers, over SSH, and on a CI runner, none of which have
//!   an unlocked keychain.
//! - **`keychain`** - the OS credential store. Secrets never touch disk in
//!   Leviath's own files, so a stolen `~/.leviath` directory yields nothing, and
//!   on macOS the OS gates access per-application.
//!
//! `file` stays the default deliberately. A keychain that is unavailable is not
//! a degraded experience, it is a broken one - every inference fails with no
//! obvious cause - and the environments where Leviath is most useful are exactly
//! the ones least likely to have a working credential store. Opting in is one
//! line; being unable to opt out would be a support burden.
//!
//! This module is the platform-neutral half: the vocabulary (which backend, what
//! an account is called) and the [`CredentialStore`] trait everything is written
//! against. The OS binding lives in `leviath-sys`, with the rest of the
//! platform-specific code and its own no-store fallback.

use std::collections::BTreeMap;

/// The service name every Leviath credential is filed under.
///
/// One service, many accounts: this is what a user sees if they open Keychain
/// Access or `secret-tool` and search for it.
pub const SERVICE: &str = "dev.leviath.lev";

/// Which backend holds secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStoreKind {
    /// `~/.leviath/config.toml` and `~/.leviath/mcp-auth.json`, mode `0600`.
    #[default]
    File,
    /// The OS credential store.
    Keychain,
}

impl CredentialStoreKind {
    /// True when secrets belong in the OS store rather than in Leviath's files.
    pub fn is_keychain(self) -> bool {
        matches!(self, Self::Keychain)
    }
}

/// The account name for a provider's API key.
///
/// Namespaced so a future non-provider secret cannot collide with a provider
/// literally named `github`, and so `lev auth status` can tell the two apart
/// without a second lookup.
pub fn provider_account(provider: &str) -> String {
    format!("provider/{provider}")
}

/// The account name for an MCP server's stored OAuth grant.
pub fn mcp_account(server: &str) -> String {
    format!("mcp/{server}")
}

/// A place secrets can be put and taken back out.
///
/// A trait rather than a concrete type so the config and MCP-auth layers can be
/// tested against an in-memory store without reaching the OS, and so a future
/// backend (a remote secret manager, say) has somewhere to land.
pub trait CredentialStore: Send + Sync {
    /// The stored secret for `account`, or `None` if there is none.
    ///
    /// A *missing* entry is `Ok(None)`; an unreachable or locked store is `Err`.
    /// The distinction matters: collapsing a locked keychain into `None` would
    /// surface to the user as "no API key configured" and send them to re-run
    /// `lev setup` instead of unlocking their keychain.
    fn get(&self, account: &str) -> Result<Option<String>, String>;

    /// Store `secret` under `account`, replacing anything already there.
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;

    /// Remove `account`, reporting whether anything was actually removed, so a
    /// caller can clean up unconditionally.
    fn delete(&self, account: &str) -> Result<bool, String>;

    /// Every one of `accounts` that is present, with its secret.
    ///
    /// The OS stores offer no portable "list everything under this service"
    /// operation, so the caller supplies the accounts to look for - it knows
    /// them: the provider list is fixed and the MCP server list comes from the
    /// config.
    ///
    /// An entry the store cannot return is skipped rather than aborting the
    /// whole read: one stale keychain item should not stop a user from running.
    fn read_all(&self, accounts: &[String]) -> BTreeMap<String, String> {
        accounts
            .iter()
            .filter_map(|a| match self.get(a) {
                Ok(Some(secret)) => Some((a.clone(), secret)),
                _ => None,
            })
            .collect()
    }
}

/// An in-memory [`CredentialStore`], for tests and for callers that want the
/// keychain code paths without a keychain.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: std::sync::Mutex<BTreeMap<String, String>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.lock().get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        self.lock().insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        Ok(self.lock().remove(account).is_some())
    }
}

impl MemoryStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        // Poisoning here would mean a test panicked while holding the lock; the
        // stored secrets are still perfectly readable, and turning that into a
        // second failure hides the first.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_namespaced_by_kind() {
        assert_eq!(provider_account("anthropic"), "provider/anthropic");
        assert_eq!(mcp_account("github"), "mcp/github");
        assert_ne!(provider_account("x"), mcp_account("x"));
    }

    /// The default has to stay `file`: it is the only backend that works
    /// headless, in a container, and over SSH.
    #[test]
    fn file_is_the_default_backend() {
        assert_eq!(CredentialStoreKind::default(), CredentialStoreKind::File);
        assert!(!CredentialStoreKind::default().is_keychain());
        assert!(CredentialStoreKind::Keychain.is_keychain());
    }

    #[test]
    fn the_kind_round_trips_through_config_syntax() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Section {
            credential_store: CredentialStoreKind,
        }

        let parsed: Section = toml::from_str("credential_store = \"keychain\"\n").unwrap();
        assert_eq!(parsed.credential_store, CredentialStoreKind::Keychain);
        assert_eq!(
            toml::to_string(&Section {
                credential_store: CredentialStoreKind::File
            })
            .unwrap()
            .trim(),
            "credential_store = \"file\""
        );
        assert!(toml::from_str::<Section>("credential_store = \"vault\"\n").is_err());
    }

    #[test]
    fn the_memory_store_round_trips_and_reports_absence() {
        let store = MemoryStore::new();
        assert_eq!(store.get("a").unwrap(), None);
        store.set("a", "one").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("one"));
        store.set("a", "two").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("two"));
        assert!(store.delete("a").unwrap(), "it was there");
        assert!(!store.delete("a").unwrap(), "and now it is not");
        assert!(format!("{:?}", MemoryStore::default()).contains("MemoryStore"));
    }

    /// `read_all` is the default trait method every backend inherits: present
    /// accounts come back, absent ones are simply not in the map.
    #[test]
    fn read_all_returns_only_the_accounts_that_exist() {
        let store = MemoryStore::new();
        store.set(&provider_account("openai"), "sk-openai").unwrap();

        let found = store.read_all(&[
            provider_account("openai"),
            provider_account("google"),
            mcp_account("github"),
        ]);

        assert_eq!(found.len(), 1);
        assert_eq!(found[&provider_account("openai")], "sk-openai");
    }

    /// One unreadable entry must not blank out the rest.
    #[test]
    fn read_all_skips_an_entry_the_store_cannot_return() {
        struct Broken;
        impl CredentialStore for Broken {
            fn get(&self, account: &str) -> Result<Option<String>, String> {
                if account == "good" {
                    Ok(Some("v".into()))
                } else {
                    Err("locked".into())
                }
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn delete(&self, _: &str) -> Result<bool, String> {
                Ok(false)
            }
        }

        let found = Broken.read_all(&["good".into(), "bad".into()]);
        assert_eq!(found.len(), 1, "the readable entry still comes back");
        assert!(found.contains_key("good"));

        // `read_all` is the default trait method; the other two are the stub's
        // own, and a store implementing the trait has to answer all three.
        assert!(Broken.set("good", "v").is_ok());
        assert!(!Broken.delete("good").unwrap());
    }

    /// A poisoned lock must not turn a readable store into a second panic.
    #[test]
    fn the_memory_store_survives_a_poisoned_lock() {
        let store = std::sync::Arc::new(MemoryStore::new());
        store.set("a", "one").unwrap();

        let poisoner = std::sync::Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("poison the lock");
        })
        .join();

        assert_eq!(store.get("a").unwrap().as_deref(), Some("one"));
    }
}
