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
/// `Debug` is hand-written (below) so the tokens cannot be printed.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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

/// Hand-written so the access and refresh tokens can never be printed.
///
/// These are live credentials for a third-party server, and this struct is
/// carried through error paths that format their context — one `{:?}` would put
/// them in the logs. The endpoints and expiry are the useful part of a debug
/// line anyway.
impl std::fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerAuth")
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                match self.refresh_token {
                    Some(_) => &"<redacted>",
                    None => &"<none>",
                },
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
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
        // `write_private`, not `fs::write` + `chmod`. The two-step version left
        // this file — access *and refresh* tokens for every MCP server — at the
        // umask default (typically 0644) between the write and the mode change,
        // so every save had a moment where any local user could read it.
        leviath_sys::write_private(path, content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write MCP auth store: {}", e))?;
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

/// Leviath's data root, from the shared resolver.
///
/// This used to be a local copy that read `LEVIATH_HOME` as the `.leviath`
/// directory *itself*, while the CLI reads it as the user home and appends
/// `.leviath`. With the override set — which is how every test and every
/// sandboxed run works — the OAuth token store therefore landed in a different
/// directory from the config naming those very servers. The default
/// (`~/.leviath/mcp-auth.json`) is unchanged.
fn leviath_home() -> Option<PathBuf> {
    leviath_core::data_dir()
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

    /// The tokens are live credentials for a third-party server, and this
    /// struct is carried through error paths that format their context.
    #[test]
    fn debug_output_never_contains_the_tokens() {
        // Distinctive values: the shared `sample()` uses "at"/"rt", which occur
        // as substrings of "authorization_endpoint" and would make this pass or
        // fail for the wrong reason.
        let mut auth = sample();
        auth.access_token = "ACCESS-TOKEN-SECRET".to_string();
        auth.refresh_token = Some("REFRESH-TOKEN-SECRET".to_string());

        let rendered = format!("{auth:?}");
        assert!(
            !rendered.contains("ACCESS-TOKEN-SECRET"),
            "access token leaked: {rendered}"
        );
        assert!(
            !rendered.contains("REFRESH-TOKEN-SECRET"),
            "refresh token leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // The parts that make a debug line useful survive.
        assert!(rendered.contains("auth.example.com"), "{rendered}");
        assert!(rendered.contains("client-123"), "{rendered}");
    }

    #[test]
    fn debug_distinguishes_an_absent_refresh_token() {
        let mut auth = sample();
        auth.refresh_token = None;
        assert!(format!("{auth:?}").contains("<none>"));
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

    /// Access *and refresh* tokens for every MCP server. Written with the mode
    /// already applied, so there is no window — however brief — where another
    /// local user could read them. The previous `fs::write` + `chmod` left the
    /// file at the umask default (typically 0644) in between, on every save.
    #[cfg(unix)]
    #[test]
    fn saving_never_leaves_the_token_store_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");

        let mut store = AuthStore::default();
        store.set("s", sample());
        store.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // And a re-save over a file that became permissive tightens it again.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        store.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // ─── default_path / LEVIATH_HOME ──────────────────────────────────────

    /// `LEVIATH_HOME` names the *home*, and `.leviath` is appended — the same
    /// reading the CLI's config, runs dir, agents dir and control socket use.
    ///
    /// This asserted `<LEVIATH_HOME>/mcp-auth.json` before, because this module
    /// carried its own copy of the resolver that treated the override as the
    /// `.leviath` directory itself. With the override set — which is how every
    /// test and every sandboxed run works — the OAuth token store therefore sat
    /// in a different directory from the config naming those very servers.
    #[test]
    fn default_path_honors_leviath_home() {
        temp_env::with_var("LEVIATH_HOME", Some("/tmp/lev-home-test"), || {
            assert_eq!(
                AuthStore::default_path(),
                Some(PathBuf::from("/tmp/lev-home-test/.leviath/mcp-auth.json"))
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
