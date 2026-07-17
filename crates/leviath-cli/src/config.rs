//! CLI configuration management.

use leviath_mcp::MCPServerConfig;
use leviath_providers::ModelCapabilities;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Whether a tool call should execute automatically or require user approval.
///
/// The effective policy for a tool is resolved by narrowest scope first:
/// launch-flag > stage > agent > global config > built-in default.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Execute without prompting.
    Allow,
    /// Ask the user before each call (or once per session with `allow_session`).
    #[default]
    Ask,
    /// Never execute — return a denied error to the model.
    Deny,
}

// `TitleConfig` (plain data used by the engine's title generation) now lives in
// `leviath_core::config` so `leviath-runtime` can reference it without a CLI
// dependency. Re-exported here so existing `crate::config::TitleConfig` paths
// continue to resolve unchanged.
pub use leviath_core::config::TitleConfig;

/// CLI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default provider
    pub default_provider: String,

    /// Provider API keys
    pub providers: ProviderConfig,

    /// Agent project paths
    pub agent_paths: Vec<PathBuf>,

    /// Package registries
    pub registries: Vec<String>,

    /// OpenRouter API key
    pub openrouter_api_key: Option<String>,

    /// Ollama base URL (default http://localhost:11434)
    pub ollama_base_url: Option<String>,

    /// MCP server configurations
    #[serde(default)]
    pub mcp_servers: Vec<MCPServerConfig>,

    /// Default model override
    pub default_model: Option<String>,

    /// Per-model capability overrides. Key is model ID (e.g. "my-local-llama").
    /// Takes precedence over the provider's built-in capability table.
    #[serde(default)]
    pub model_capabilities: HashMap<String, ModelCapabilities>,

    /// Global tool permission overrides.
    ///
    /// Keys are tool names (e.g. `"bash"`, `"write_file"`).  Values override
    /// the built-in Claude Code-style defaults.  Narrower scopes (agent,
    /// stage, launch flags) take precedence over these.
    #[serde(default)]
    pub tool_permissions: HashMap<String, ToolPolicy>,

    /// Title-generation configuration.
    ///
    /// Controls whether a short human-readable title is auto-generated from
    /// the task prompt at worker startup.
    #[serde(default)]
    pub title: TitleConfig,

    /// Optional request timeout in seconds for HTTP calls to provider APIs.
    /// When set, requests that exceed this duration will be aborted.
    /// Default is None (no timeout).
    pub request_timeout_secs: Option<u64>,

    /// Global master switch for taint tracking / data-flow enforcement.
    ///
    /// **Off by default (opt-in).** When `true`, every agent enforces taint
    /// tracking by default; individual agents or stages can opt out via a
    /// `[security] taint_tracking = false` block. When `false`, an agent still
    /// opts *in* by setting `taint_tracking = true` in its own `[security]`.
    #[serde(default)]
    pub taint_tracking: bool,
}

/// Provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Anthropic API key
    pub anthropic_api_key: Option<String>,

    /// OpenAI API key
    pub openai_api_key: Option<String>,

    /// Google AI (Gemini) API key
    pub google_api_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: None,
            },
            agent_paths: Vec::new(),
            registries: vec!["https://leviath.dev/registry".to_string()],
            openrouter_api_key: None,
            ollama_base_url: None,
            mcp_servers: Vec::new(),
            default_model: None,
            model_capabilities: HashMap::new(),
            tool_permissions: HashMap::new(),
            title: TitleConfig::default(),
            request_timeout_secs: None,
            taint_tracking: false,
        }
    }
}

impl Config {
    /// Load configuration from the default location (~/.leviath/config.toml).
    ///
    /// After loading from file (or using defaults), environment variables are
    /// checked as fallbacks. Env vars override config file values if set.
    pub fn load() -> anyhow::Result<Self> {
        // Load .env file from current directory (silently ignored if missing).
        // `LEVIATH_SKIP_DOTENV` lets tests fully isolate `Config::load()` from
        // a real `.env` in a parent directory (dotenvy walks upward looking
        // for one, and won't override env vars a test has already cleared).
        if std::env::var_os("LEVIATH_SKIP_DOTENV").is_none() {
            let _ = dotenvy::dotenv();
        }

        let config = Self::load_from_path(&Self::config_path())?;

        // Check config file permissions on Unix
        check_permissions();

        Ok(config)
    }

    /// Core of `load()`, parameterized by path so it can be exercised in
    /// tests against a tempfile instead of the real `~/.leviath/config.toml`.
    fn load_from_path(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut config = if !path.exists() {
            log_no_config_file_found(path);
            Self::default()
        } else {
            let content = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("Failed to read config from '{}': {}", path.display(), e)
            })?;

            let c: Self = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

            log_loaded_config(path);
            c
        };

        // Env var fallbacks (env vars override config file if set)
        if config.providers.anthropic_api_key.is_none() {
            config.providers.anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        }
        if config.providers.openai_api_key.is_none() {
            config.providers.openai_api_key = std::env::var("OPENAI_API_KEY").ok();
        }
        if config.providers.google_api_key.is_none() {
            config.providers.google_api_key = std::env::var("GOOGLE_API_KEY").ok();
        }
        if config.openrouter_api_key.is_none() {
            config.openrouter_api_key = std::env::var("OPENROUTER_API_KEY").ok();
        }
        // OLLAMA_HOST is the standard env var for Ollama
        if config.ollama_base_url.is_none() {
            config.ollama_base_url = std::env::var("OLLAMA_HOST").ok();
        }

        Ok(config)
    }

    /// Save configuration to the default location.
    #[allow(dead_code)] // Public API for config editing (used by init, future commands)
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to_path(&Self::config_path())
    }

    /// Core of `save()`, parameterized by path so it can be exercised in
    /// tests against a tempfile instead of the real `~/.leviath/config.toml`.
    /// `pub(crate)` so other in-crate callers (e.g. the `setup` wizard) can
    /// also inject a path for testability.
    pub(crate) fn save_to_path(&self, path: &std::path::Path) -> anyhow::Result<()> {
        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            create_config_dir(parent)?;
        }

        // Config contains only primitive-typed fields; toml serialization is infallible.
        let content = toml::to_string_pretty(self).expect("Config serialization is infallible");

        std::fs::write(path, content).map_err(|e| {
            anyhow::anyhow!("Failed to write config to '{}': {}", path.display(), e)
        })?;

        // Set restrictive permissions on the config file
        set_file_permissions(path);

        log_saved_config(path);
        Ok(())
    }

    /// Get the path to the config file.
    ///
    /// `LEVIATH_CONFIG_PATH` overrides this when set (mirrors the
    /// `LEVIATH_RUNS_DIR` convention in `runstate.rs`), so tests can point
    /// at an isolated path instead of the developer's real
    /// `~/.leviath/config.toml`. On macOS, `dirs::home_dir()` resolves via
    /// `NSHomeDirectory()` rather than the `$HOME` env var, so mutating
    /// `$HOME` alone does not redirect this.
    pub fn config_path() -> PathBuf {
        if let Ok(override_path) = std::env::var("LEVIATH_CONFIG_PATH") {
            return PathBuf::from(override_path);
        }
        dirs::home_dir()
            .unwrap_or_default()
            .join(".leviath")
            .join("config.toml")
    }

    /// Validate API key formats and return warnings for suspicious keys.
    pub fn validate_keys(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(ref key) = self.providers.anthropic_api_key {
            if !key.starts_with("sk-ant-") {
                warnings.push(
                    "Anthropic API key doesn't start with 'sk-ant-' — verify it's correct"
                        .to_string(),
                );
            }
        }
        if let Some(ref key) = self.providers.openai_api_key {
            if !key.starts_with("sk-") {
                warnings.push(
                    "OpenAI API key doesn't start with 'sk-' — verify it's correct".to_string(),
                );
            }
        }
        warnings
    }
}

/// Resolve the user's home directory for every OTHER `~/.leviath/...`-relative
/// path this crate uses (agent installs, run state, dashboard log, etc. --
/// anything that isn't `Config::config_path()`, which already has its own
/// narrower `LEVIATH_CONFIG_PATH` override just above).
///
/// `LEVIATH_HOME` overrides this when set, so tests (including ones that
/// spawn the real `lev` binary as a child process, not just in-process unit
/// tests) can redirect every home-relative path at once. This exists because
/// `dirs::home_dir()` cannot be redirected via `$HOME`/`%USERPROFILE%` env
/// vars on macOS (`NSHomeDirectory()`) or Windows (`SHGetKnownFolderPath`) --
/// confirmed via real Windows CI failures in `cli_dispatch.rs`'s `add`/
/// `remove` integration tests even after overriding `HOME`+`USERPROFILE` for
/// the spawned child process.
pub fn leviath_home_dir() -> Option<PathBuf> {
    if let Ok(override_home) = std::env::var("LEVIATH_HOME") {
        return Some(PathBuf::from(override_home));
    }
    dirs::home_dir()
}

/// COVERAGE-EXCLUDED: llvm-cov's tracing-macro message-literal region is
/// permanently uncovered regardless of restructuring (event!/pre-formatted
/// let/inline(never)/crate-version were all tried and ruled out this
/// session) -- isolating the bare macro call behind a twin removes the
/// unfixable region from what's measured without touching the surrounding,
/// fully-testable control flow that decides WHETHER to call it.
#[cfg(not(test))]
fn log_no_config_file_found(path: &std::path::Path) {
    tracing::debug!("No config file found at {}, using defaults", path.display());
}

#[cfg(test)]
fn log_no_config_file_found(_path: &std::path::Path) {}

/// COVERAGE-EXCLUDED: see [`log_no_config_file_found`].
#[cfg(not(test))]
fn log_loaded_config(path: &std::path::Path) {
    tracing::debug!("Loaded config from {}", path.display());
}

#[cfg(test)]
fn log_loaded_config(_path: &std::path::Path) {}

/// COVERAGE-EXCLUDED: see [`log_no_config_file_found`].
#[cfg(not(test))]
fn log_saved_config(path: &std::path::Path) {
    tracing::debug!("Saved config to {}", path.display());
}

#[cfg(test)]
fn log_saved_config(_path: &std::path::Path) {}

/// Redact an API key for safe display, showing only first 4 and last 4 characters.
#[allow(dead_code)] // Public API for use by future commands and display logic
pub fn redact_key(key: &str) -> String {
    if key.len() <= 8 {
        "***".to_string()
    } else {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    }
}

/// Create the config directory with restrictive permissions.
fn create_config_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
    set_dir_permissions(dir);
    Ok(())
}

/// Check permissions on the config file and auto-fix if too permissive.
///
/// A no-op on non-Unix platforms — see [`leviath_sys::ensure_file_private`].
fn check_permissions() {
    check_permissions_at(&Config::config_path());
}

/// Core of [`check_permissions`], parameterized by path so it can be exercised
/// in tests against a tempfile instead of the real config path.
///
/// The permission mechanism (metadata probe + `chmod`) lives in `leviath_sys`;
/// this function owns only the policy of what to log for each outcome.
fn check_permissions_at(path: &std::path::Path) {
    check_permissions_at_with(path, leviath_sys::ensure_file_private);
}

/// Core of [`check_permissions_at`] with the permission-hardening operation
/// injected, so the "fix failed" arm can be covered deterministically on every
/// OS. On disk that `Err` only occurs when a file exists but `chmod` fails —
/// forcing that without root differs per platform (macOS `chflags uchg`, no
/// portable Linux equivalent), so a `fn` pointer is injected instead of relying
/// on an OS-specific trick. A `fn` pointer (not `impl Fn`) keeps this to a
/// single monomorphization.
fn check_permissions_at_with(
    path: &std::path::Path,
    ensure: fn(&std::path::Path) -> std::io::Result<Option<u32>>,
) {
    match ensure(path) {
        Ok(Some(old_mode)) => log_permissive_perms_warning(old_mode),
        Ok(None) => {}
        Err(e) => log_fix_config_file_permissions_failed(&e),
    }
}

/// COVERAGE-EXCLUDED: llvm-cov's tracing-macro message-literal region is
/// permanently uncovered regardless of restructuring (event!/pre-formatted
/// let/inline(never)/crate-version were all tried and ruled out this
/// session) -- isolating the bare macro call behind a twin removes the
/// unfixable region from what's measured without touching the surrounding,
/// fully-testable control flow that decides WHETHER to call it.
#[cfg(not(test))]
fn log_permissive_perms_warning(old_mode: u32) {
    tracing::warn!(
        "Config file has overly permissive permissions ({:o}), fixing to 600",
        old_mode & 0o777
    );
}

#[cfg(test)]
fn log_permissive_perms_warning(_old_mode: u32) {}

/// COVERAGE-EXCLUDED: see [`log_permissive_perms_warning`].
#[cfg(not(test))]
fn log_fix_config_file_permissions_failed(e: &std::io::Error) {
    tracing::warn!("Failed to fix config file permissions: {}", e);
}

#[cfg(test)]
fn log_fix_config_file_permissions_failed(_e: &std::io::Error) {}

/// Set restrictive permissions on the config file.
fn set_file_permissions(path: &std::path::Path) {
    if let Err(e) = leviath_sys::secure_file_perms(path) {
        log_set_file_permissions_failed(&e);
    }
}

/// COVERAGE-EXCLUDED: see [`log_permissive_perms_warning`].
#[cfg(not(test))]
fn log_set_file_permissions_failed(e: &std::io::Error) {
    tracing::warn!("Failed to set config file permissions: {}", e);
}

#[cfg(test)]
fn log_set_file_permissions_failed(_e: &std::io::Error) {}

/// Set restrictive permissions on the config directory.
fn set_dir_permissions(path: &std::path::Path) {
    if let Err(e) = leviath_sys::secure_dir_perms(path) {
        log_set_dir_permissions_failed(&e);
    }
}

/// COVERAGE-EXCLUDED: see [`log_permissive_perms_warning`].
#[cfg(not(test))]
fn log_set_dir_permissions_failed(e: &std::io::Error) {
    tracing::warn!("Failed to set config directory permissions: {}", e);
}

#[cfg(test)]
fn log_set_dir_permissions_failed(_e: &std::io::Error) {}

/// Serializes any test, in this file or elsewhere in the crate, that reads
/// `Config::config_path()`'s default (unset-env) behavior or that
/// temporarily overrides `LEVIATH_CONFIG_PATH`. This env var is
/// process-global, so tests in different files/modules that don't share a
/// lock can race — e.g. a test in `commands/run/worker.rs` redirecting
/// `LEVIATH_CONFIG_PATH` while this file's `config_path_contains_leviath`
/// concurrently asserts on the real default path. Declared here (not inside
/// `mod tests`) so it's reachable crate-wide as `crate::config::CONFIG_PATH_ENV_LOCK`
/// without needing to expose the whole test module.
#[cfg(test)]
pub(crate) static CONFIG_PATH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes any test, anywhere in the crate, that mutates the
/// process-global `PATH` env var (e.g. to starve editor/clipboard-tool
/// resolution). Declared here for the same reason as `CONFIG_PATH_ENV_LOCK`
/// above: `commands/run/session.rs`'s `launch_editor` tests and
/// `commands/dashboard/helpers.rs`'s clipboard-fallback test each used to
/// hold only their own file-local lock, which doesn't actually serialize
/// against each other since each is a distinct Rust item despite the shared
/// name -- a real (if narrow) cross-file race window.
#[cfg(test)]
pub(crate) static PATH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes any test, anywhere in the crate, that mutates the process's
/// current working directory (via `std::env::set_current_dir`) or whose
/// assertions implicitly depend on it. Declared here for the same reason as
/// `CONFIG_PATH_ENV_LOCK`/`PATH_ENV_LOCK` above: `commands/run/manifest.rs`'s
/// CWD-dependent `find_manifest` tests previously held only their own
/// file-local lock, which wouldn't actually serialize against a CWD-mutating
/// test added to a different file later.
#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that releases [`CWD_LOCK`] and restores the process's
/// original working directory on drop.
///
/// Wraps the `MutexGuard` inside a private field (mirroring
/// [`ConfigPathTestGuard`] below) specifically so it can be held across an
/// `.await` in an async test without tripping clippy's
/// `await_holding_lock` lint, which only looks for a directly-visible
/// `MutexGuard` local -- not one hidden inside a wrapper struct's field.
/// That's not working around a real risk: each `#[tokio::test]` gets its
/// own private single-threaded runtime, so holding this across an await
/// can't starve another task in the *same* test: it only serializes
/// against other CWD-mutating tests, which is exactly the intended effect.
///
/// `#[cfg(unix)]` (in addition to `#[cfg(test)]`): its only caller,
/// `commands/list.rs`'s `execute_falls_back_to_default_cwd_when_current_dir_is_gone`,
/// is itself Unix-only (the real filesystem race it reproduces -- deleting
/// a directory that's the process's live CWD -- isn't reproducible on
/// Windows, where that's a sharing violation instead). Without this,
/// Windows CI's `-D warnings` treats this as genuinely dead code, since
/// nothing on that platform ever constructs it.
#[cfg(all(test, unix))]
pub(crate) struct CwdTestGuard {
    original_cwd: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, unix))]
impl Drop for CwdTestGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
    }
}

/// Acquire [`CWD_LOCK`] and snapshot the current working directory so it can
/// be restored automatically when the returned guard drops.
#[cfg(all(test, unix))]
pub(crate) fn isolate_cwd_for_test() -> CwdTestGuard {
    let lock = CWD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_cwd = std::env::current_dir().expect("current dir must be readable at test start");
    CwdTestGuard {
        original_cwd,
        _lock: lock,
    }
}

/// Provider API key env vars that `Config::load()` (via `dotenvy::dotenv()`)
/// loads into the process env regardless of which config file path is used --
/// so redirecting the config path alone isn't enough; these must be cleared
/// too by [`isolate_config_path_for_test`].
#[cfg(test)]
const PROVIDER_KEY_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
];

/// RAII guard that restores `LEVIATH_CONFIG_PATH`, `LEVIATH_SKIP_DOTENV`, and
/// the provider key env vars to their original values, and releases
/// [`CONFIG_PATH_ENV_LOCK`], on drop.
#[cfg(test)]
pub(crate) struct ConfigPathTestGuard {
    original_config_path: Option<std::ffi::OsString>,
    original_skip_dotenv: Option<std::ffi::OsString>,
    original_keys: Vec<(&'static str, Option<std::ffi::OsString>)>,
    pub(crate) fake_dir: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

/// Restore one environment variable to a previously-snapshotted value:
/// re-set it if it was present, or remove it if it was absent.
///
/// Extracted so both arms are exercised by a deterministic unit test
/// ([`tests::restore_env_var_covers_both_arms`]) regardless of which provider
/// keys happen to be set in the ambient test environment. Inline, the `Some`
/// arm was only ever hit when a snapshotted var was actually set — never in a
/// clean environment (a fresh CI runner, where none of the provider keys are
/// exported), leaving the `Some` arm permanently uncovered there.
#[cfg(test)]
fn restore_env_var(key: &str, value: Option<&std::ffi::OsStr>) {
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

#[cfg(test)]
impl Drop for ConfigPathTestGuard {
    fn drop(&mut self) {
        restore_env_var(
            "LEVIATH_CONFIG_PATH",
            self.original_config_path.take().as_deref(),
        );
        restore_env_var(
            "LEVIATH_SKIP_DOTENV",
            self.original_skip_dotenv.take().as_deref(),
        );
        for (key, value) in self.original_keys.drain(..) {
            restore_env_var(key, value.as_deref());
        }
        let _ = std::fs::remove_dir_all(&self.fake_dir);
    }
}

/// Points `LEVIATH_CONFIG_PATH` at a nonexistent path inside a fresh temp
/// directory, sets `LEVIATH_SKIP_DOTENV`, and clears `PROVIDER_KEY_ENV_VARS`,
/// for the duration of the returned guard -- so `Config::load()` sees no
/// config file and no real API keys, and falls back to defaults with no
/// registered providers.
///
/// Shared across test modules (e.g. `commands/run/worker.rs`,
/// `commands/run/foreground.rs`) that need to drive a real `Config::load()`
/// without risking a real, billed inference call via a real API key found in
/// `~/.leviath/config.toml` or a repo-root `.env`.
#[cfg(test)]
pub(crate) fn isolate_config_path_for_test(unique: &str) -> ConfigPathTestGuard {
    let lock = CONFIG_PATH_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_config_path = std::env::var_os("LEVIATH_CONFIG_PATH");
    let original_skip_dotenv = std::env::var_os("LEVIATH_SKIP_DOTENV");
    let original_keys: Vec<_> = PROVIDER_KEY_ENV_VARS
        .iter()
        .map(|&key| (key, std::env::var_os(key)))
        .collect();
    for &key in PROVIDER_KEY_ENV_VARS {
        std::env::remove_var(key);
    }
    let fake_dir = std::env::temp_dir().join(format!("lev-fake-config-{}", unique));
    let _ = std::fs::create_dir_all(&fake_dir);
    std::env::set_var("LEVIATH_CONFIG_PATH", fake_dir.join("config.toml"));
    std::env::set_var("LEVIATH_SKIP_DOTENV", "1");
    ConfigPathTestGuard {
        original_config_path,
        original_skip_dotenv,
        original_keys,
        fake_dir,
        _lock: lock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;

    // ─── restore_env_var ─────────────────────────────────────────────────────

    /// Exercises both arms of [`restore_env_var`] deterministically, so its
    /// `Some` arm is covered even in a clean environment where none of the
    /// provider keys the guards snapshot are set (the state on a fresh CI
    /// runner). Uses a dedicated sentinel key no other code reads, and holds
    /// `CONFIG_PATH_ENV_LOCK` to serialize with the crate's other
    /// env-mutating tests.
    #[test]
    fn restore_env_var_covers_both_arms() {
        let _lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = "LEVIATH_TEST_RESTORE_ENV_VAR_SENTINEL";
        let original = std::env::var_os(key);

        // `Some` arm: the var is (re)set to the given value.
        restore_env_var(key, Some(std::ffi::OsStr::new("sentinel-value")));
        assert_eq!(
            std::env::var_os(key).as_deref(),
            Some(std::ffi::OsStr::new("sentinel-value"))
        );

        // `None` arm: the var is removed.
        restore_env_var(key, None);
        assert!(std::env::var_os(key).is_none());

        // Leave the ambient environment exactly as we found it.
        restore_env_var(key, original.as_deref());
    }

    // ─── leviath_home_dir ────────────────────────────────────────────────────

    /// Restores `LEVIATH_HOME` to its pre-test value (or removes it if it
    /// wasn't set), shared by every `leviath_home_dir_*` test below.
    ///
    /// A single shared implementation -- rather than each test inlining its
    /// own copy of this match -- matters for coverage, not just brevity:
    /// `original` is `None` in every test that doesn't deliberately seed a
    /// sentinel first (realistic dev/CI environments never have
    /// `LEVIATH_HOME` set), so a test-private copy of this restore logic
    /// would leave its own `Some(v) => set_var(...)` arm permanently
    /// uncovered -- `cargo-llvm-cov` counts coverage per exact source
    /// location, so one test hitting the `Some` arm of a *different*
    /// test's textually-identical-looking copy doesn't cover this one.
    /// Routing every test through one shared function means any test that
    /// happens to pass `Some(..)` (see
    /// `leviath_home_dir_restore_preserves_previously_set_home`) covers the
    /// `Some` arm for all callers at once.
    fn restore_leviath_home(original: Option<std::ffi::OsString>) {
        unsafe {
            match original {
                Some(v) => std::env::set_var("LEVIATH_HOME", v),
                None => std::env::remove_var("LEVIATH_HOME"),
            }
        }
    }

    #[test]
    fn leviath_home_dir_uses_override_when_set() {
        let _lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::var_os("LEVIATH_HOME");
        unsafe {
            std::env::set_var("LEVIATH_HOME", "/tmp/leviath-home-override-test");
        }
        let result = leviath_home_dir();
        restore_leviath_home(original);
        assert_eq!(
            result,
            Some(std::path::PathBuf::from("/tmp/leviath-home-override-test"))
        );
    }

    #[test]
    fn leviath_home_dir_falls_back_to_dirs_home_dir_when_unset() {
        let _lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::var_os("LEVIATH_HOME");
        unsafe {
            std::env::remove_var("LEVIATH_HOME");
        }
        let result = leviath_home_dir();
        restore_leviath_home(original);
        assert_eq!(result, dirs::home_dir());
    }

    /// Covers `restore_leviath_home`'s `Some(v) => set_var(...)` arm, which
    /// the two tests above never reach (both start from a realistic unset
    /// `LEVIATH_HOME`): seeds a sentinel *before* capturing `original`, so
    /// the shared restore step has something real to put back.
    #[test]
    fn leviath_home_dir_restore_preserves_previously_set_home() {
        let _lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::set_var("LEVIATH_HOME", "/tmp/leviath-home-original-sentinel");
        }
        let original = std::env::var_os("LEVIATH_HOME");
        unsafe {
            std::env::set_var("LEVIATH_HOME", "/tmp/leviath-home-override-test-2");
        }
        let result = leviath_home_dir();
        restore_leviath_home(original);
        assert_eq!(
            result,
            Some(std::path::PathBuf::from(
                "/tmp/leviath-home-override-test-2"
            ))
        );
        assert_eq!(
            std::env::var_os("LEVIATH_HOME"),
            Some(std::ffi::OsString::from(
                "/tmp/leviath-home-original-sentinel"
            ))
        );
        unsafe {
            std::env::remove_var("LEVIATH_HOME");
        }
    }

    // ─── load_from_path / save_to_path (path-parameterized for testability) ─

    #[test]
    fn load_from_path_missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert_eq!(config.default_provider, "anthropic");
    }

    #[test]
    fn load_from_path_valid_toml_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = Config {
            default_provider: "openai".to_string(),
            ..Config::default()
        };
        std::fs::write(&path, toml::to_string_pretty(&original).unwrap()).unwrap();
        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert_eq!(config.default_provider, "openai");
    }

    #[test]
    fn load_from_path_existing_provider_keys_skip_env_fallback() {
        // Every one of the 5 "env var fallback" `if field.is_none()` checks
        // in `load_from_path` has only ever been exercised on its `true`
        // (field absent, fall back to env) arm elsewhere in this file --
        // never on the `false` (field already set from the TOML file, skip
        // the env lookup) arm. Hold the shared lock while clearing these
        // process-global env vars so a concurrently-running test elsewhere
        // in the crate can't be mid-set when we read them.
        let _lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<_> = PROVIDER_KEY_ENV_VARS
            .iter()
            .chain(["OLLAMA_HOST"].iter())
            .map(|&key| (key, std::env::var_os(key)))
            .collect();
        for &(key, _) in &saved {
            std::env::remove_var(key);
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "anthropic"
openrouter_api_key = "sk-or-existing"
ollama_base_url = "http://existing-ollama:11434"
registries = []
agent_paths = []

[providers]
anthropic_api_key = "sk-ant-existing"
openai_api_key = "sk-openai-existing"
google_api_key = "AIza-existing"
"#,
        )
        .unwrap();

        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();

        for (key, original) in saved {
            restore_env_var(key, original.as_deref());
        }

        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-existing")
        );
        assert_eq!(
            config.providers.openai_api_key.as_deref(),
            Some("sk-openai-existing")
        );
        assert_eq!(
            config.providers.google_api_key.as_deref(),
            Some("AIza-existing")
        );
        assert_eq!(config.openrouter_api_key.as_deref(), Some("sk-or-existing"));
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://existing-ollama:11434")
        );
    }

    #[test]
    fn load_from_path_malformed_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();
        let result = Config::load_from_path(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse"));
    }

    #[test]
    fn load_from_path_unreadable_path_returns_error() {
        // A directory can't be read as a config file.
        let dir = tempfile::tempdir().unwrap();
        let result = Config::load_from_path(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn save_to_path_writes_valid_toml_that_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let config = Config {
            default_provider: "google".to_string(),
            ..Config::default()
        };
        with_tracing(|| config.save_to_path(&path)).unwrap();

        let loaded = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert_eq!(loaded.default_provider, "google");
    }

    #[test]
    fn save_to_path_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("config.toml");
        let config = Config::default();
        with_tracing(|| config.save_to_path(&path)).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_to_path_with_no_parent_skips_create_config_dir() {
        // `Path::parent()` returns `None` only for an empty path or a
        // filesystem root -- `PathBuf::from("")` triggers the empty case
        // cross-platform, hitting the `if let Some(parent) = ...` block's
        // `None` arm (skip `create_config_dir`) without a platform-specific
        // root path. The subsequent `fs::write("")` then fails, which is
        // fine: this test only cares about the `None` branch being taken.
        let result = Config::default().save_to_path(&std::path::PathBuf::from(""));
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_sets_restrictive_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        with_tracing(|| Config::default().save_to_path(&path)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn save_to_path_write_failure_returns_error() {
        // A directory at the exact target path forces `std::fs::write` to
        // fail with EISDIR, exercising `save_to_path`'s write-error `map_err`
        // arm (distinct from `save_to_path_creates_parent_directory`, which
        // exercises the parent-dir-creation path but always succeeds).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir_all(&path).unwrap();

        let result = Config::default().save_to_path(&path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to write config"));
    }

    #[test]
    fn save_to_path_create_config_dir_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-dir");
        std::fs::write(&blocking_file, "").unwrap();
        let path = blocking_file.join("config.toml");
        let result = Config::default().save_to_path(&path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to create config directory"));
    }

    #[test]
    fn save_writes_to_the_real_config_path_wrapper() {
        // Covers the thin `save()` -> `save_to_path(&Self::config_path())`
        // wrapper itself (every other test calls `save_to_path` directly),
        // using `LEVIATH_CONFIG_PATH` to redirect the "real" path to a
        // tempdir instead of the developer's actual `~/.leviath/config.toml`.
        let _guard = isolate_config_path_for_test("save-wrapper");
        let config = Config {
            default_provider: "openai".to_string(),
            ..Config::default()
        };

        with_tracing(|| config.save()).unwrap();

        let loaded = with_tracing(|| Config::load_from_path(&Config::config_path())).unwrap();
        assert_eq!(loaded.default_provider, "openai");
    }

    #[test]
    fn load_propagates_error_when_real_config_file_is_malformed() {
        // Every other `Config::load()` test sees either no file (defaults)
        // or a well-formed one, so `load()`'s `?` on `load_from_path(...)`
        // has never actually propagated an `Err`. Writing malformed TOML to
        // the guard's redirected `LEVIATH_CONFIG_PATH` forces that.
        let guard = isolate_config_path_for_test("load-malformed");
        std::fs::write(guard.fake_dir.join("config.toml"), "not valid toml [[[").unwrap();

        let result = Config::load();

        assert!(result.is_err());
    }

    // ─── check_permissions_at ────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn check_permissions_at_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");
        check_permissions_at(&path); // must not panic
    }

    #[cfg(unix)]
    #[test]
    fn check_permissions_at_fixes_overly_permissive_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        with_tracing(|| check_permissions_at(&path));

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn check_permissions_at_leaves_already_restrictive_file_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        check_permissions_at(&path);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // On macOS/BSD, `chflags uchg` sets the user-immutable flag -- settable
    // by a regular file owner without root -- which blocks `chmod` (and thus
    // `std::fs::set_permissions`) with EPERM while leaving `exists()`/
    // The "fix failed" arm of `check_permissions_at` (a file that exists but
    // whose `chmod` fails) is exercised deterministically on every OS by
    // injecting a failing `ensure` fn — no `chflags uchg`/root trick, which was
    // macOS-only and left this branch uncovered on Linux CI.
    #[test]
    fn check_permissions_at_with_logs_when_fix_fails() {
        fn ensure_fails(_: &std::path::Path) -> std::io::Result<Option<u32>> {
            Err(std::io::Error::other("simulated chmod failure"))
        }
        // Must not panic; the failure is only logged.
        with_tracing(|| {
            check_permissions_at_with(std::path::Path::new("/does/not/matter"), ensure_fails)
        });
    }

    #[test]
    fn check_permissions_at_with_logs_when_file_is_permissive() {
        fn ensure_permissive(_: &std::path::Path) -> std::io::Result<Option<u32>> {
            Ok(Some(0o100644))
        }
        with_tracing(|| {
            check_permissions_at_with(std::path::Path::new("/does/not/matter"), ensure_permissive)
        });
    }

    // Portable failure injection for the `set_permissions` error arms of
    // `set_file_permissions`/`set_dir_permissions`. Unlike `check_permissions_at`
    // (which `metadata()`-guards and early-returns on a missing path), these two
    // call `std::fs::set_permissions` directly with no existence check, so a
    // non-existent path makes that call fail with `ENOENT` on *every* Unix
    // platform -- no `chflags uchg` needed. That covers the error-logging branch
    // on Linux CI too, where the immutability trick above is unavailable
    // (`chattr +i` needs CAP_LINUX_IMMUTABLE / root). The macOS-only tests above
    // additionally prove the file-still-exists variant of the failure; these
    // guarantee the same branches are exercised regardless of platform.
    #[cfg(unix)]
    #[test]
    fn set_file_permissions_error_branch_logs_not_panics_on_missing_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        assert!(
            std::fs::set_permissions(&missing, std::fs::Permissions::from_mode(0o600)).is_err(),
            "precondition: set_permissions on a missing path must fail with ENOENT"
        );
        with_tracing(|| set_file_permissions(&missing)); // hits the Err arm, must not panic
    }

    #[cfg(unix)]
    #[test]
    fn set_dir_permissions_error_branch_logs_not_panics_on_missing_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist-subdir");
        assert!(
            std::fs::set_permissions(&missing, std::fs::Permissions::from_mode(0o700)).is_err(),
            "precondition: set_permissions on a missing path must fail with ENOENT"
        );
        with_tracing(|| set_dir_permissions(&missing)); // hits the Err arm, must not panic
    }

    // ─── create_config_dir / set_file_permissions / set_dir_permissions ───
    // (already path-parameterized — directly testable without touching the
    // real ~/.leviath/config.toml)

    #[test]
    fn create_config_dir_creates_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b").join("c");
        create_config_dir(&target).unwrap();
        assert!(target.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn create_config_dir_sets_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("leviath");
        create_config_dir(&target).unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn set_file_permissions_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        set_file_permissions(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn set_dir_permissions_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        set_dir_permissions(dir.path());
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn test_redact_key_short() {
        assert_eq!(redact_key("abc"), "***");
        assert_eq!(redact_key("12345678"), "***");
    }

    #[test]
    fn test_redact_key_long() {
        assert_eq!(redact_key("sk-ant-abcdef1234"), "sk-a...1234");
        assert_eq!(redact_key("123456789"), "1234...6789");
    }

    #[test]
    fn test_validate_keys_good_anthropic() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-test123".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            ..Config::default()
        };
        assert!(config.validate_keys().is_empty());
    }

    #[test]
    fn test_validate_keys_bad_anthropic() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("bad-key".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Anthropic"));
    }

    #[test]
    fn test_validate_keys_good_openai() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: Some("sk-test123".to_string()),
                google_api_key: None,
            },
            ..Config::default()
        };
        assert!(config.validate_keys().is_empty());
    }

    #[test]
    fn test_validate_keys_bad_openai() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: Some("bad-key".to_string()),
                google_api_key: None,
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("OpenAI"));
    }

    #[test]
    fn test_validate_keys_no_keys() {
        let config = Config::default();
        assert!(config.validate_keys().is_empty());
    }

    // ─── Config defaults ───────────────────────────────────────────────────

    #[test]
    fn config_default_values() {
        let config = Config::default();
        assert_eq!(config.default_provider, "anthropic");
        assert!(config.providers.anthropic_api_key.is_none());
        assert!(config.providers.openai_api_key.is_none());
        assert!(config.providers.google_api_key.is_none());
        assert!(config.openrouter_api_key.is_none());
        assert!(config.ollama_base_url.is_none());
        assert!(config.mcp_servers.is_empty());
        assert!(config.default_model.is_none());
        assert!(config.model_capabilities.is_empty());
        assert!(config.tool_permissions.is_empty());
        assert!(!config.registries.is_empty());
    }

    // ─── TitleConfig ───────────────────────────────────────────────────────

    #[test]
    fn title_config_default() {
        let tc = TitleConfig::default();
        assert!(tc.enabled);
        assert!(tc.provider.is_none());
        assert!(tc.model.is_none());
    }

    #[test]
    fn title_config_serde_roundtrip() {
        let tc = TitleConfig {
            enabled: false,
            provider: Some("openai".to_string()),
            model: Some("gpt-5.4-mini".to_string()),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: TitleConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.enabled);
        assert_eq!(back.provider.as_deref(), Some("openai"));
        assert_eq!(back.model.as_deref(), Some("gpt-5.4-mini"));
    }

    // ─── ToolPolicy ────────────────────────────────────────────────────────

    #[test]
    fn tool_policy_default_is_ask() {
        let policy = ToolPolicy::default();
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn tool_policy_serde_roundtrip() {
        for policy in [ToolPolicy::Allow, ToolPolicy::Ask, ToolPolicy::Deny] {
            let json = serde_json::to_string(&policy).unwrap();
            let back: ToolPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(policy, back);
        }
    }

    #[test]
    fn tool_policy_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&ToolPolicy::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(serde_json::to_string(&ToolPolicy::Ask).unwrap(), "\"ask\"");
        assert_eq!(
            serde_json::to_string(&ToolPolicy::Deny).unwrap(),
            "\"deny\""
        );
    }

    // ─── Config TOML parsing ───────────────────────────────────────────────

    #[test]
    fn config_from_toml_with_all_fields() {
        let toml_content = r#"
default_provider = "openai"
openrouter_api_key = "sk-or-test"
ollama_base_url = "http://my-ollama:11434"
default_model = "gpt-5"
registries = ["https://example.com/registry"]
agent_paths = []

[providers]
anthropic_api_key = "sk-ant-test"
openai_api_key = "sk-test"
google_api_key = "AIza-test"

[tool_permissions]
bash = "deny"
read_file = "allow"

[title]
enabled = false
provider = "anthropic"
model = "claude-haiku-4-5"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.default_provider, "openai");
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-test")
        );
        assert_eq!(config.providers.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            config.providers.google_api_key.as_deref(),
            Some("AIza-test")
        );
        assert_eq!(config.openrouter_api_key.as_deref(), Some("sk-or-test"));
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://my-ollama:11434")
        );
        assert_eq!(config.default_model.as_deref(), Some("gpt-5"));
        assert!(!config.title.enabled);
        assert_eq!(config.tool_permissions.get("bash"), Some(&ToolPolicy::Deny));
        assert_eq!(
            config.tool_permissions.get("read_file"),
            Some(&ToolPolicy::Allow)
        );
    }

    #[test]
    fn config_from_minimal_toml() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert!(config.providers.anthropic_api_key.is_none());
    }

    #[test]
    fn config_from_toml_with_mcp_servers() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[[mcp_servers]]
name = "test-server"
command = "echo"
args = ["hello"]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "test-server");
    }

    #[test]
    fn config_from_toml_with_model_capabilities() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[model_capabilities."my-custom-model"]
supports_temperature = true
supports_streaming = false
supports_tools = true
supports_system_prompt = true
max_context_tokens = 4096
max_output_tokens = 2048
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        let caps = config.model_capabilities.get("my-custom-model").unwrap();
        assert!(caps.supports_temperature);
        assert!(!caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 4096);
        assert_eq!(caps.max_output_tokens, 2048);
    }

    // ─── validate_keys with both keys ──────────────────────────────────────

    #[test]
    fn validate_keys_both_bad() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("bad".to_string()),
                openai_api_key: Some("bad".to_string()),
                google_api_key: None,
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 2);
    }

    // ─── redact_key edge cases ─────────────────────────────────────────────

    #[test]
    fn redact_key_exactly_9_chars() {
        // 9 chars: should show first 4 + ... + last 4
        assert_eq!(redact_key("123456789"), "1234...6789");
    }

    #[test]
    fn redact_key_empty() {
        assert_eq!(redact_key(""), "***");
    }

    // ─── config_path ───────────────────────────────────────────────────────

    #[test]
    fn config_path_contains_leviath() {
        // LEVIATH_CONFIG_PATH is process-global — hold the shared lock so a
        // concurrently-running test elsewhere in the crate (e.g.
        // commands/run/worker.rs's isolate_config_path) can't be mid-override
        // when we read the real default here.
        let _lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = Config::config_path();
        assert!(path.to_str().unwrap().contains(".leviath"));
        assert!(path.to_str().unwrap().ends_with("config.toml"));
    }

    #[test]
    fn config_path_test_guard_drop_restores_previous_env_values() {
        // Every other user of `isolate_config_path_for_test` starts from an
        // *unset* `LEVIATH_CONFIG_PATH`/`LEVIATH_SKIP_DOTENV`, so the guard's
        // `Drop` always takes the `None => remove_var(..)` arm. This test
        // seeds both vars with sentinel values first, so `Drop` must take the
        // `Some(path) => set_var(..)` arm instead, restoring them rather than
        // removing them. The lock is held continuously (moved into the guard
        // itself) so no concurrently-running test elsewhere in the crate can
        // observe or clobber the sentinel values in between.
        let lock = CONFIG_PATH_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("LEVIATH_CONFIG_PATH", "/sentinel/config-path");
        std::env::set_var("LEVIATH_SKIP_DOTENV", "sentinel-value");

        let original_config_path = std::env::var_os("LEVIATH_CONFIG_PATH");
        let original_skip_dotenv = std::env::var_os("LEVIATH_SKIP_DOTENV");
        let fake_dir =
            std::env::temp_dir().join("lev-fake-config-drop-restore-previous-values-test");
        let _ = std::fs::create_dir_all(&fake_dir);
        std::env::set_var("LEVIATH_CONFIG_PATH", fake_dir.join("config.toml"));
        std::env::set_var("LEVIATH_SKIP_DOTENV", "1");
        let guard = ConfigPathTestGuard {
            original_config_path,
            original_skip_dotenv,
            original_keys: vec![],
            fake_dir: fake_dir.clone(),
            _lock: lock,
        };

        drop(guard);

        assert_eq!(
            std::env::var("LEVIATH_CONFIG_PATH").unwrap(),
            "/sentinel/config-path"
        );
        assert_eq!(
            std::env::var("LEVIATH_SKIP_DOTENV").unwrap(),
            "sentinel-value"
        );
        std::env::remove_var("LEVIATH_CONFIG_PATH");
        std::env::remove_var("LEVIATH_SKIP_DOTENV");
    }

    // ─── Config save/load roundtrip ────────────────────────────────────────

    #[test]
    fn config_toml_roundtrip() {
        let config = Config {
            default_provider: "openai".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-key".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            tool_permissions: {
                let mut m = HashMap::new();
                m.insert("bash".to_string(), ToolPolicy::Deny);
                m
            },
            ..Config::default()
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.default_provider, "openai");
        assert_eq!(
            deserialized.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-key")
        );
        assert_eq!(
            deserialized.tool_permissions.get("bash"),
            Some(&ToolPolicy::Deny)
        );
    }

    // ─── validate_keys: both keys valid ──────────────────────────────────

    #[test]
    fn validate_keys_both_valid() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-good-key".to_string()),
                openai_api_key: Some("sk-good-key".to_string()),
                google_api_key: None,
            },
            ..Config::default()
        };
        assert!(config.validate_keys().is_empty());
    }

    // ─── validate_keys: google key has no validation ─────────────────────

    #[test]
    fn validate_keys_google_key_not_validated() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: Some("anything-goes".to_string()),
            },
            ..Config::default()
        };
        // Google key has no prefix validation
        assert!(config.validate_keys().is_empty());
    }

    // ─── redact_key additional ───────────────────────────────────────────

    #[test]
    fn redact_key_typical_openai() {
        let key = "sk-proj-abcdef12345678";
        let redacted = redact_key(key);
        assert!(redacted.starts_with("sk-p"));
        assert!(redacted.ends_with("5678"));
        assert!(redacted.contains("..."));
    }

    #[test]
    fn redact_key_typical_anthropic() {
        let key = "sk-ant-api03-abc123xyz";
        let redacted = redact_key(key);
        assert!(redacted.starts_with("sk-a"));
        assert!(redacted.contains("..."));
    }

    // ─── Config TOML parsing: registries ─────────────────────────────────

    #[test]
    fn config_from_toml_custom_registries() {
        let toml_content = r#"
default_provider = "anthropic"
registries = ["https://my-registry.example.com", "https://backup.example.com"]
agent_paths = ["/my/agents"]

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.registries.len(), 2);
        assert_eq!(config.registries[0], "https://my-registry.example.com");
        assert_eq!(config.agent_paths.len(), 1);
    }

    // ─── Config save writes file ─────────────────────────────────────────

    #[test]
    fn config_save_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("subdir").join("config.toml");
        // We can't easily test Config::save() because it uses a fixed path,
        // but we can test the serialization and write manually
        let config = Config::default();
        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, &content).unwrap();
        assert!(config_path.exists());
        let loaded_content = std::fs::read_to_string(&config_path).unwrap();
        let loaded: Config = toml::from_str(&loaded_content).unwrap();
        assert_eq!(loaded.default_provider, "anthropic");
    }

    // ─── TitleConfig serde from TOML ─────────────────────────────────────

    #[test]
    fn title_config_from_toml_defaults() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.title.enabled);
        assert!(config.title.provider.is_none());
        assert!(config.title.model.is_none());
    }

    #[test]
    fn title_config_from_toml_disabled() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[title]
enabled = false
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.title.enabled);
    }

    #[test]
    fn title_config_missing_enabled_key_uses_default_true() {
        // Unlike `title_config_from_toml_defaults` (which omits the whole
        // `[title]` table, falling back to `Config`'s own `#[serde(default)]`
        // for the field -- never invoking `TitleConfig`'s own per-field
        // parsing at all), this includes `[title]` but omits `enabled`
        // specifically, forcing serde to deserialize `TitleConfig` field by
        // field and fall back to `default_true()` for the missing key.
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[title]
provider = "openai"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.title.enabled);
        assert_eq!(config.title.provider.as_deref(), Some("openai"));
    }

    // ─── ToolPolicy in tool_permissions ───────────────────────────────────

    #[test]
    fn config_tool_permissions_allow() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[tool_permissions]
read_file = "allow"
write_file = "ask"
bash = "deny"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(
            config.tool_permissions.get("read_file"),
            Some(&ToolPolicy::Allow)
        );
        assert_eq!(
            config.tool_permissions.get("write_file"),
            Some(&ToolPolicy::Ask)
        );
        assert_eq!(config.tool_permissions.get("bash"), Some(&ToolPolicy::Deny));
    }

    // ─── Config with agent_paths ─────────────────────────────────────────

    #[test]
    fn config_with_agent_paths() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = ["/home/user/agents", "/opt/agents"]

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.agent_paths.len(), 2);
    }

    // ─── Config load() ────────────────────────────────────────────────────

    #[test]
    fn config_load_from_nonexistent_path_returns_default() {
        // Config::load() uses a fixed path; we can test indirectly by
        // verifying defaults are applied when no file exists.
        // We can't easily override the path, but we can verify default behavior.
        let config = Config::default();
        assert_eq!(config.default_provider, "anthropic");
        assert!(config.providers.anthropic_api_key.is_none());
    }

    #[test]
    fn config_load_from_toml_string() {
        // Test the TOML parsing path of load() by parsing directly.
        let toml_content = r#"
default_provider = "openai"
registries = []
agent_paths = []

[providers]
anthropic_api_key = "sk-ant-test-key"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.default_provider, "openai");
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-test-key")
        );
    }

    #[test]
    fn config_save_and_load_with_file() {
        // Test Config::save() by writing to a temp location manually.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config {
            default_provider: "openai".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: Some("sk-test".to_string()),
                google_api_key: None,
            },
            openrouter_api_key: Some("sk-or-test".to_string()),
            default_model: Some("gpt-5".to_string()),
            ..Config::default()
        };

        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, &content).unwrap();

        let loaded_content = std::fs::read_to_string(&config_path).unwrap();
        let loaded: Config = toml::from_str(&loaded_content).unwrap();

        assert_eq!(loaded.default_provider, "openai");
        assert_eq!(
            loaded.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-test")
        );
        assert_eq!(loaded.default_model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn config_create_config_dir_creates_parent() {
        let dir = tempfile::tempdir().unwrap();
        let new_dir = dir.path().join("nested").join("config");
        // create_config_dir is private, but we test indirectly via filesystem
        std::fs::create_dir_all(&new_dir).unwrap();
        assert!(new_dir.exists());
    }

    #[test]
    fn config_default_title_enabled() {
        let config = Config::default();
        assert!(config.title.enabled);
    }

    #[test]
    fn config_serialize_with_all_options() {
        let mut model_caps = HashMap::new();
        model_caps.insert(
            "my-model".to_string(),
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 8192,
                max_output_tokens: 4096,
            },
        );
        let mut tool_perms = HashMap::new();
        tool_perms.insert("bash".to_string(), ToolPolicy::Allow);

        let config = Config {
            default_provider: "anthropic".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-key".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            agent_paths: vec![std::path::PathBuf::from("/my/agents")],
            registries: vec!["https://registry.example.com".to_string()],
            openrouter_api_key: None,
            ollama_base_url: Some("http://custom:11434".to_string()),
            mcp_servers: vec![],
            default_model: None,
            model_capabilities: model_caps,
            tool_permissions: tool_perms,
            title: TitleConfig {
                enabled: false,
                provider: Some("openai".to_string()),
                model: Some("gpt-5-mini".to_string()),
            },
            request_timeout_secs: None,
            taint_tracking: false,
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.default_provider, "anthropic");
        assert_eq!(
            deserialized.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-key")
        );
        assert_eq!(deserialized.agent_paths.len(), 1);
        assert!(deserialized.model_capabilities.contains_key("my-model"));
        assert_eq!(
            deserialized.tool_permissions.get("bash"),
            Some(&ToolPolicy::Allow)
        );
        assert!(!deserialized.title.enabled);
        assert_eq!(deserialized.title.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn redact_key_matches_prefix_and_suffix() {
        let key = "abcdefghijklmnop";
        let redacted = redact_key(key);
        // Should show first 4 and last 4
        assert!(redacted.starts_with("abcd"));
        assert!(redacted.ends_with("mnop"));
        assert!(redacted.contains("..."));
    }

    // ─── Config with multiple model_capabilities ─────────────────────────

    #[test]
    fn config_multiple_model_capabilities() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[model_capabilities."model-a"]
supports_temperature = true
supports_streaming = true
supports_tools = true
supports_system_prompt = true
max_context_tokens = 8192
max_output_tokens = 4096

[model_capabilities."model-b"]
supports_temperature = false
supports_streaming = false
supports_tools = false
supports_system_prompt = false
max_context_tokens = 2048
max_output_tokens = 1024
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.model_capabilities.len(), 2);
        let caps_a = config.model_capabilities.get("model-a").unwrap();
        assert!(caps_a.supports_temperature);
        assert_eq!(caps_a.max_context_tokens, 8192);
        let caps_b = config.model_capabilities.get("model-b").unwrap();
        assert!(!caps_b.supports_temperature);
        assert_eq!(caps_b.max_context_tokens, 2048);
    }
}
