//! On-disk store for MCP OAuth tokens and client registrations.
//!
//! Kept out of `config.toml` deliberately: the config is round-tripped and
//! rewritten by the CLI, and access/refresh tokens have no business passing
//! through that path. This file is written `0600`, the same trust level as the
//! API keys already in the config.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The authorization-server endpoints and client id learned for one server,
/// plus its current tokens. Everything needed to refresh without re-running
/// discovery.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerAuth {
    /// The `resource` value bound into every token (RFC 8707): the canonical
    /// MCP endpoint URL.
    pub resource: String,
    /// Authorization-server issuer.
    pub issuer: String,
    /// Authorization endpoint (browser).
    pub authorization_endpoint: String,
    /// Token endpoint.
    pub token_endpoint: String,
    /// Dynamic-registration client id, reused across logins.
    pub client_id: String,
    /// Current bearer token.
    pub access_token: String,
    /// Refresh token, when the server issued one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Absolute expiry, as a Unix timestamp in seconds. `0` means "unknown",
    /// treated as already expired so a refresh is attempted.
    #[serde(default)]
    pub expires_at: u64,
    /// Granted scope, for display.
    #[serde(default)]
    pub scope: String,
}

impl ServerAuth {
    /// Would this token be expired at `now` (Unix seconds), within a refresh
    /// margin?
    ///
    /// The 60-second margin refreshes a token that is about to lapse rather
    /// than letting the very next request fail and retry. An `expires_at` of 0
    /// (unknown) always reads as expired.
    pub fn is_expired_at(&self, now: u64) -> bool {
        self.expires_at <= now.saturating_add(60)
    }
}

/// The whole store: one [`ServerAuth`] per configured server name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default)]
    servers: HashMap<String, ServerAuth>,
}

impl AuthStore {
    /// Load the store from `path`, or an empty one if it does not exist.
    ///
    /// A missing file is normal (nobody has logged in yet); a present but
    /// unreadable or corrupt file is an error, because silently starting empty
    /// would drop working credentials.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read MCP auth store: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("MCP auth store is corrupt: {}", e))
    }

    /// Write the store to `path` with owner-only permissions.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        create_parent_dir(path)?;
        let content = serde_json::to_string_pretty(self).expect("AuthStore is always serializable");
        std::fs::write(path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write MCP auth store: {}", e))?;
        // Tokens are secrets: lock the file down to the owner, same as the
        // config that holds provider API keys. Setting the mode on a file we
        // just wrote and own cannot fail (and is a no-op on non-Unix), so this
        // is an assertion, not a fallible step.
        leviath_sys::secure_file_perms(path)
            .expect("securing a just-written, owned file cannot fail");
        Ok(())
    }

    /// The default store path, honoring `LEVIATH_HOME` like every other
    /// home-relative path this workspace uses.
    ///
    /// `None` when no home directory can be resolved and no override is set —
    /// the same shape the CLI's own home resolver returns, so the caller
    /// decides how to report a missing home rather than this silently picking
    /// a surprising fallback.
    pub fn default_path() -> Option<PathBuf> {
        leviath_home().map(|home| home.join("mcp-auth.json"))
    }

    /// Look up a server's stored auth.
    pub fn get(&self, server: &str) -> Option<&ServerAuth> {
        self.servers.get(server)
    }

    /// Insert or replace a server's auth.
    pub fn set(&mut self, server: &str, auth: ServerAuth) {
        self.servers.insert(server.to_string(), auth);
    }

    /// Remove a server's auth, returning whether anything was removed.
    pub fn remove(&mut self, server: &str) -> bool {
        self.servers.remove(server).is_some()
    }

    /// Names of every server with stored auth.
    pub fn server_names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }
}

/// Ensure the directory holding `path` exists.
///
/// A path with no parent, or an empty-string parent (a bare filename), needs
/// no directory created — those collapse to a no-op so the caller has a single
/// fallible step to reason about.
fn create_parent_dir(path: &Path) -> anyhow::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create MCP auth store directory: {}", e)),
        _ => Ok(()),
    }
}

/// `$LEVIATH_HOME`, else `~/.leviath`, else `None`.
///
/// Mirrors the resolution the CLI's config uses. `dirs::home_dir` cannot be
/// redirected by `$HOME` on macOS/Windows, so `LEVIATH_HOME` is the single
/// override every home-relative path (including this one) honors under test.
fn leviath_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("LEVIATH_HOME") {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir().map(|home| home.join(".leviath"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServerAuth {
        ServerAuth {
            resource: "https://mcp.example.com/mcp".to_string(),
            issuer: "https://auth.example.com".to_string(),
            authorization_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            client_id: "client-123".to_string(),
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            expires_at: 10_000,
            scope: "openid".to_string(),
        }
    }

    // ─── expiry ───────────────────────────────────────────────────────────

    #[test]
    fn a_token_well_in_the_future_is_not_expired() {
        assert!(!sample().is_expired_at(5_000));
    }

    #[test]
    fn a_token_inside_the_refresh_margin_is_expired() {
        // 9_950 + 60 = 10_010 > 10_000, so it is treated as expired early.
        assert!(sample().is_expired_at(9_950));
    }

    #[test]
    fn a_lapsed_token_is_expired() {
        assert!(sample().is_expired_at(20_000));
    }

    #[test]
    fn an_unknown_expiry_reads_as_expired() {
        let mut auth = sample();
        auth.expires_at = 0;
        assert!(auth.is_expired_at(0));
    }

    // ─── round-trip ───────────────────────────────────────────────────────

    #[test]
    fn set_get_and_remove() {
        let mut store = AuthStore::default();
        assert!(store.get("s").is_none());
        store.set("s", sample());
        assert_eq!(store.get("s"), Some(&sample()));
        assert_eq!(store.server_names(), vec!["s"]);
        assert!(store.remove("s"));
        assert!(!store.remove("s"), "second remove finds nothing");
        assert!(store.get("s").is_none());
    }

    #[test]
    fn loading_a_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::load(&dir.path().join("nope.json")).unwrap();
        assert!(store.server_names().is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("mcp-auth.json");
        let mut store = AuthStore::default();
        store.set("s", sample());
        store.save(&path).unwrap();

        let loaded = AuthStore::load(&path).unwrap();
        assert_eq!(loaded.get("s"), Some(&sample()));
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_reset() {
        // Silently starting empty here would drop a user's working tokens.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = AuthStore::load(&path).expect_err("corrupt store must error");
        assert!(err.to_string().contains("corrupt"), "got: {err}");
    }

    #[test]
    fn a_saved_refresh_token_survives_but_none_is_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let mut auth = sample();
        auth.refresh_token = None;
        let mut store = AuthStore::default();
        store.set("s", auth);
        store.save(&path).unwrap();
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("refresh_token")
        );
        assert_eq!(
            AuthStore::load(&path)
                .unwrap()
                .get("s")
                .unwrap()
                .refresh_token,
            None
        );
    }

    #[test]
    fn create_parent_dir_makes_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("store.json");
        create_parent_dir(&path).unwrap();
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn create_parent_dir_is_a_no_op_without_a_real_parent() {
        // A bare filename (empty parent) and a root path (no parent) both skip.
        create_parent_dir(Path::new("store.json")).unwrap();
        create_parent_dir(Path::new("/")).unwrap();
    }

    #[test]
    fn loading_an_unreadable_path_errors() {
        // A directory exists at the path, so `exists()` is true but reading it
        // as a file fails — the read-error arm, distinct from a missing file.
        let dir = tempfile::tempdir().unwrap();
        let as_dir = dir.path().join("store-is-a-dir");
        std::fs::create_dir(&as_dir).unwrap();
        let err = AuthStore::load(&as_dir).expect_err("reading a dir must fail");
        assert!(
            err.to_string().contains("read MCP auth store"),
            "got: {err}"
        );
    }

    #[test]
    fn saving_onto_a_directory_errors() {
        // The target path itself is a directory, so the write fails.
        let dir = tempfile::tempdir().unwrap();
        let as_dir = dir.path().join("target-is-a-dir");
        std::fs::create_dir(&as_dir).unwrap();
        let err = AuthStore::default()
            .save(&as_dir)
            .expect_err("writing onto a directory must fail");
        assert!(
            err.to_string().contains("write MCP auth store"),
            "got: {err}"
        );
    }

    #[test]
    fn saving_under_a_non_directory_parent_errors() {
        // The parent path is a *file*, so create_dir_all can't make it a
        // directory — reaching the directory-creation error arm.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let path = blocker.join("mcp-auth.json");
        let err = AuthStore::default()
            .save(&path)
            .expect_err("cannot create a dir under a file");
        assert!(err.to_string().contains("directory"), "got: {err}");
    }

    #[test]
    fn saved_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        AuthStore::default().save(&path).unwrap();
        // Tokens are secrets; assert the file is locked to the owner.
        let mode = leviath_sys::ensure_file_private(&path).unwrap();
        assert!(
            mode.is_none(),
            "already private after save, got remediation {mode:?}"
        );
    }

    // ─── default_path / LEVIATH_HOME ──────────────────────────────────────

    #[test]
    fn default_path_honors_leviath_home() {
        temp_env::with_var("LEVIATH_HOME", Some("/tmp/lev-home-test"), || {
            assert_eq!(
                AuthStore::default_path(),
                Some(PathBuf::from("/tmp/lev-home-test/mcp-auth.json"))
            );
        });
    }

    #[test]
    fn default_path_falls_back_to_dot_leviath() {
        temp_env::with_var_unset("LEVIATH_HOME", || {
            // In CI a home directory always resolves; assert the shape.
            let path = AuthStore::default_path().expect("home should resolve");
            assert!(path.ends_with(".leviath/mcp-auth.json"), "got: {path:?}");
        });
    }
}
