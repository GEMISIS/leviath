//! Telling whether a remembered provider check was made with the key a
//! provider holds now, without writing down anything a key can be found from.
//!
//! The capability cache records each provider's last check (see
//! [`leviath_providers::ProviderCheck`]) and every surface reads it, so a
//! check has to say which credential it was made with: a key changed since
//! must not show as checked. The cache itself leaves the machine, though.
//! `lev rage` packs it into a bug report, and a plain hash of a short token
//! typed for a self-hosted server can be reversed by guessing.
//!
//! So a check carries an HMAC-SHA256 of the credential under a random key
//! that is generated once per install and kept in its own owner-only file
//! beside the cache, which no bug report packs. Without that file a
//! fingerprint is noise; with it, each surface on this machine computes the
//! same one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The file the check key lives in, beside the cache.
const KEY_FILE: &str = "provider-check.key";

/// Hex digits kept from the HMAC: 128 bits, far past any accidental match.
const FINGERPRINT_HEX: usize = 32;

/// This install's key for fingerprinting credentials.
#[derive(Clone)]
pub(crate) struct CheckKey([u8; 32]);

impl CheckKey {
    /// Where the key for the cache at `cache` is kept.
    pub(crate) fn path_beside(cache: &Path) -> PathBuf {
        cache.with_file_name(KEY_FILE)
    }

    /// The key beside the cache at `cache`, if one has been made and reads
    /// back whole.
    pub(crate) fn load(cache: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path_beside(cache)).ok()?;
        let bytes = hex::decode(text.trim()).ok()?;
        Some(Self(bytes.try_into().ok()?))
    }

    /// The key beside the cache at `cache`, made on first use.
    ///
    /// Two processes making it at once each write one, and each then reads
    /// back whichever landed, so both fingerprint with the same key. `None`
    /// when it cannot be written, which leaves every keyed check unmatched
    /// ("not checked yet") rather than failing anything.
    pub(crate) fn load_or_create(cache: &Path) -> Option<Self> {
        if let Some(key) = Self::load(cache) {
            return Some(key);
        }
        use rand::RngExt as _;
        let fresh: [u8; 32] = rand::rng().random();
        let path = Self::path_beside(cache);
        path.parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .ok()?;
        leviath_sys::write_atomic(&path, hex::encode(fresh).as_bytes(), Some(0o600)).ok()?;
        Self::load(cache)
    }

    /// The fingerprint of `secret` under this key.
    pub(crate) fn fingerprint(&self, secret: &str) -> String {
        let mut mac =
            <Hmac<Sha256>>::new_from_slice(&self.0).expect("HMAC takes a key of any length");
        mac.update(secret.as_bytes());
        let mut hex = hex::encode(mac.finalize().into_bytes());
        hex.truncate(FINGERPRINT_HEX);
        hex
    }
}

/// The fingerprint of every keyed provider `config` names, under the check
/// key beside the cache at `cache`, for the checks a surface records. Empty
/// when there is no cache path or no key can be made: every keyed check is
/// then recorded with no fingerprint and matches nothing.
pub(crate) fn fingerprints(
    cache: Option<&Path>,
    config: &crate::config::Config,
) -> HashMap<String, String> {
    let Some(key) = cache.and_then(CheckKey::load_or_create) else {
        return HashMap::new();
    };
    crate::commands::run::session::provider_creds_from_config(config)
        .into_iter()
        .filter_map(|c| Some((c.name, key.fingerprint(c.api_key.as_deref()?))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_made_once_kept_owner_only_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("data").join("model_capabilities.json");
        assert!(CheckKey::load(&cache).is_none(), "nothing made yet");
        let made = CheckKey::load_or_create(&cache).expect("made");
        let again = CheckKey::load_or_create(&cache).expect("read back");
        assert_eq!(made.fingerprint("sk-a"), again.fingerprint("sk-a"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(CheckKey::path_beside(&cache))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn fingerprints_differ_by_secret_and_by_install_and_never_hold_the_secret() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        let a = CheckKey::load_or_create(&one.path().join("c.json")).unwrap();
        let b = CheckKey::load_or_create(&two.path().join("c.json")).unwrap();
        let print = a.fingerprint("tok");
        assert_eq!(print.len(), FINGERPRINT_HEX);
        assert_ne!(print, a.fingerprint("tok2"));
        assert_ne!(
            print,
            b.fingerprint("tok"),
            "another install's key gives another print"
        );
        assert!(!print.contains("tok"));
    }

    #[test]
    fn a_damaged_key_file_reads_as_none_and_an_unwritable_one_makes_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("c.json");
        std::fs::write(CheckKey::path_beside(&cache), "not hex").unwrap();
        assert!(CheckKey::load(&cache).is_none());
        std::fs::write(CheckKey::path_beside(&cache), "abcd").unwrap();
        assert!(CheckKey::load(&cache).is_none(), "too short");
        // A file where the data directory would have to be.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        assert!(CheckKey::load_or_create(&blocker.join("sub").join("c.json")).is_none());
        // A directory where the key file would go: it reads as nothing and
        // cannot be written over.
        let walled = dir.path().join("walled").join("c.json");
        std::fs::create_dir_all(CheckKey::path_beside(&walled)).unwrap();
        assert!(CheckKey::load_or_create(&walled).is_none());
    }

    #[test]
    fn a_config_fingerprints_its_keyed_providers_and_not_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("c.json");
        let mut config = crate::config::Config::default();
        config.providers.anthropic_api_key = Some("sk-ant".to_string());
        config.providers.ollama_enabled = true;
        let prints = fingerprints(Some(&cache), &config);
        let key = CheckKey::load(&cache).expect("made on first use");
        assert_eq!(prints.get("anthropic"), Some(&key.fingerprint("sk-ant")));
        assert!(!prints.contains_key("ollama"), "no key, no fingerprint");
        assert!(fingerprints(None, &config).is_empty());
    }
}
