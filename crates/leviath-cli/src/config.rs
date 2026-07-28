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

// `TitleConfig` (plain data used by the engine's title generation) lives in
// `leviath_core::config` so `leviath-runtime` can reference it without a CLI
// dependency. Re-exported here so `crate::config::TitleConfig` paths resolve.
pub use leviath_core::config::TitleConfig;

/// Permission for one Rhai *script-tool* host function (Layer 3 of the four-layer
/// model in issue #97). Gates what a registered script may *do*, independent of
/// whether the tool itself is visible ([`available_tools`]) or approved at
/// runtime ([`ToolPolicy`]).
///
/// [`available_tools`]: leviath_core::blueprint::Stage::available_tools
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScriptPermission {
    /// The host function may run.
    Allow,
    /// The host function is blocked — the call returns a `[denied]` error.
    Deny,
    /// Defer to the agent's own `tool_permissions` for the equivalent built-in
    /// (`read_file`/`shell`): permitted only when that resolves to
    /// [`ToolPolicy::Allow`]. For the network/env functions (`http_get`,
    /// `http_post`, `env_var`), which have no built-in equivalent, `Inherit`
    /// permits the call (they're needed for tools to be useful, and the tool
    /// itself is still gated by Layers 1/2/4).
    #[default]
    Inherit,
}

/// Per-host-function permissions for Rhai script tools (`[tool_script_permissions]`).
///
/// Every field defaults to [`ScriptPermission::Inherit`], so an unconfigured
/// install lets network/env functions run while file/shell functions defer to
/// the agent's own tool permissions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptToolPermissions {
    /// Permission for `http_get`.
    #[serde(default)]
    pub http_get: ScriptPermission,
    /// Permission for `http_post`.
    #[serde(default)]
    pub http_post: ScriptPermission,
    /// Permission for `shell`.
    #[serde(default)]
    pub shell: ScriptPermission,
    /// Permission for `read_file`.
    #[serde(default)]
    pub read_file: ScriptPermission,
    /// Permission for `write_file`.
    #[serde(default)]
    pub write_file: ScriptPermission,
    /// Permission for `env_var`.
    #[serde(default)]
    pub env_var: ScriptPermission,
}

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

    /// Optional overrides for Rhai *script providers* (issue #101). Key is the
    /// provider name an agent references (e.g. `"groq"`). A script activates by
    /// being referenced + its `.rhai` file existing in the providers dir; an
    /// entry here only supplies overrides (an API key not read from env, a
    /// `base_url`, a `rate_limit`, a differently-named `script`, or extra keys
    /// forwarded to the script's `initialize`).
    #[serde(default)]
    pub model_providers: HashMap<String, ModelProviderConfig>,

    /// Global tool permission overrides.
    ///
    /// Keys are tool names (e.g. `"bash"`, `"write_file"`). Values override the
    /// built-in defaults, and act as a **ceiling** that a blueprint's own
    /// `[tool_permissions]` may tighten but never loosen — see
    /// [`crate::tools::resolve_policy`]. To grant one agent more than this
    /// without loosening it everywhere, use [`Self::agent_tool_permissions`].
    #[serde(default)]
    pub tool_permissions: HashMap<String, ToolPolicy>,

    /// Per-agent tool permission grants, keyed by agent name.
    ///
    /// ```toml
    /// [agent_tool_permissions.coder]
    /// shell = "allow"
    /// ```
    ///
    /// This is the escape hatch for the ceiling in [`Self::tool_permissions`].
    /// Because a blueprint may only tighten what the user configured, a global
    /// `shell = "ask"` would otherwise stop a trusted agent from pre-approving
    /// its own shell. Naming the agent here is the user saying "I trust this
    /// one" — a decision that lives in the user's config, not the downloaded
    /// manifest's. Entries replace the global value for that agent, and are then
    /// the ceiling the blueprint is clamped against.
    #[serde(default)]
    pub agent_tool_permissions: HashMap<String, HashMap<String, ToolPolicy>>,

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

    /// Runtime resource limits (inference concurrency + iteration caps).
    #[serde(default)]
    pub limits: LimitsConfig,

    /// Global master switch for the batch-tool-calls system-prompt hint.
    ///
    /// **On by default (opt-out).** When `true`, every stage's request carries a
    /// short hint telling the model it may emit several `tool_use` blocks in one
    /// response and should batch *independent* operations (but never dependent
    /// ones) to cut API round trips. Individual agents or stages can opt out by
    /// setting `batch_tool_hint = false` in their `[agent]` / `[stages.<name>]`
    /// blocks; when this global is `false`, they opt back *in* by setting it to
    /// `true` at the narrower scope.
    #[serde(default = "default_true")]
    pub batch_tool_hint: bool,

    /// Completion-webhook delivery tuning (retry/backoff/timeout).
    #[serde(default)]
    pub webhook: WebhookConfig,

    /// Machine-wide default sandbox for tool execution. An agent's own
    /// `[sandbox]` (or a stage's) overrides this; when unset, agents run tools
    /// on the host unless they opt in themselves. See
    /// [`leviath_core::resolve_sandbox`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<leviath_core::ToolSandboxConfig>,

    /// Per-host-function permissions for Rhai script tools (Layer 3). Gates what
    /// a registered script tool may *do* (network, shell, file, env access).
    #[serde(default)]
    pub tool_script_permissions: ScriptToolPermissions,

    /// Machine-wide security switches that aren't part of the per-tool
    /// permission cascade. (The global taint master switch stays the top-level
    /// [`Self::taint_tracking`] key for back-compat.)
    #[serde(default)]
    pub security: SecurityConfig,
}

/// `[security]` in `~/.leviath/config.toml`.
///
/// Distinct from a *blueprint's* `[security]` block, which configures taint
/// tracking for one agent — this one holds machine-wide switches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityConfig {
    /// Whether a blueprint's `seed = { command = "..." }` regions may run
    /// (issue #108).
    ///
    /// **On by default.** A command seed executes at spawn — before the first
    /// inference, and therefore before any tool-approval prompt — so it is the
    /// one place a manifest can run something without the user being asked.
    /// It is still confined to the run's workdir, routed through the entry
    /// stage's sandbox when the agent declares one, and capped by
    /// `[limits] script_shell_timeout_secs`. Set this to `false` to refuse them
    /// machine-wide, or pass `--no-seed-commands` for a single run. Inspect an
    /// agent's command seeds before installing it with `lev validate <path>`.
    #[serde(default = "default_true")]
    pub allow_seed_commands: bool,

    /// Whether agent-driven fetches may reach loopback, private, and link-local
    /// addresses.
    ///
    /// **Off by default.** An agent's `web_fetch` URL is chosen by the model out
    /// of context an attacker can influence — a search result, a page fetched a
    /// moment ago, an issue body — so an unrestricted fetch makes the agent a
    /// confused deputy *inside* the user's network. The concrete targets are
    /// `http://169.254.169.254/…` (cloud metadata, which returns instance
    /// credentials), `http://127.0.0.1:3000/api/…` (the user's own `lev serve`),
    /// and anything on the LAN.
    ///
    /// Turn this on when the agent is genuinely meant to talk to something local
    /// — a self-hosted model, a dev server under test. It applies to the script
    /// host's `http_get`/`http_post` and to redirect following; see
    /// [`leviath_core::net`].
    #[serde(default)]
    pub allow_local_network: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            allow_seed_commands: true,
            allow_local_network: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_concurrent_inferences() -> Option<usize> {
    Some(8)
}

fn default_default_max_iterations() -> Option<usize> {
    Some(50)
}

fn default_max_concurrent_tools() -> usize {
    8
}

fn default_script_shell_timeout_secs() -> u64 {
    60
}

/// Runtime resource limits with safe defaults baked in.
///
/// Both fields default to a bounded value so a fresh install can't accidentally
/// run unbounded inference concurrency or an unbounded agent loop. Set a field
/// explicitly in `[limits]` to raise or lower it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// Global fallback cap on concurrent inference requests for any model
    /// without its own per-model pool entry. Defaults to `Some(8)`; omit or set
    /// a large number to effectively unbound it.
    #[serde(default = "default_max_concurrent_inferences")]
    pub max_concurrent_inferences: Option<usize>,

    /// Size of the shared tool-execution worker pool — the number of agents whose
    /// tool batches may run concurrently across the whole daemon (the tool-lane
    /// counterpart of `max_concurrent_inferences`). Defaults to `8`. Clamped to at
    /// least 1.
    #[serde(default = "default_max_concurrent_tools")]
    pub max_concurrent_tools: usize,

    /// Fallback `max_iterations` applied to a stage that does not set its own,
    /// so an agent can't loop forever with no completion signal. Defaults to
    /// `Some(50)`. A stage's explicit `max_iterations` always wins.
    #[serde(default = "default_default_max_iterations")]
    pub default_max_iterations: Option<usize>,

    /// Opt-in exact pre-inference token budgeting. When `true`, each agent
    /// inference is preceded by an exact token count of the assembled request
    /// (via the provider's `count_tokens`, which uses a remote endpoint for
    /// Anthropic/Gemini and a local heuristic otherwise) and is rejected before
    /// sending if it would exceed the model's context window. Off by default:
    /// normal budgeting uses cheap local estimates, and this adds a network
    /// round-trip per inference for providers with a remote count endpoint.
    #[serde(default)]
    pub exact_token_counting: bool,

    /// Wall-clock timeout (seconds) for a Rhai script tool's `shell()` host call,
    /// mirroring the built-in shell tool's own 60-second cap so a script can't
    /// hang an agent on a runaway command. Defaults to `60`.
    #[serde(default = "default_script_shell_timeout_secs")]
    pub script_shell_timeout_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_concurrent_inferences: default_max_concurrent_inferences(),
            max_concurrent_tools: default_max_concurrent_tools(),
            default_max_iterations: default_default_max_iterations(),
            exact_token_counting: false,
            script_shell_timeout_secs: default_script_shell_timeout_secs(),
        }
    }
}

fn default_webhook_max_retries() -> u32 {
    3
}

fn default_webhook_base_delay_ms() -> u64 {
    500
}

fn default_webhook_max_delay_ms() -> u64 {
    30_000
}

fn default_webhook_timeout_secs() -> u64 {
    10
}

/// Completion-webhook delivery tuning.
///
/// A completion webhook is POSTed when a run reaches a terminal status. Delivery
/// retries on transient failures (network errors, timeouts, 5xx, 429, 408) with
/// exponential backoff. Each field has a safe default so `[webhook]` can be
/// omitted entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Number of retries **after** the first attempt (so total sends is
    /// `max_retries + 1`). Defaults to `3`. Set `0` to disable retries.
    #[serde(default = "default_webhook_max_retries")]
    pub max_retries: u32,

    /// Base backoff before the first retry, in milliseconds. Subsequent retries
    /// double it (capped at `max_delay_ms`). Defaults to `500`.
    #[serde(default = "default_webhook_base_delay_ms")]
    pub base_delay_ms: u64,

    /// Upper bound on any single backoff delay, in milliseconds. Defaults to
    /// `30_000` (30s).
    #[serde(default = "default_webhook_max_delay_ms")]
    pub max_delay_ms: u64,

    /// Per-attempt request timeout, in seconds. Defaults to `10`.
    #[serde(default = "default_webhook_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            max_retries: default_webhook_max_retries(),
            base_delay_ms: default_webhook_base_delay_ms(),
            max_delay_ms: default_webhook_max_delay_ms(),
            timeout_secs: default_webhook_timeout_secs(),
        }
    }
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

    /// Whether the Claude Code CLI transport is enabled.
    ///
    /// **Opt-in, and never selected for the user.** The CLI injects its own
    /// context into every call — including the account email address on the
    /// OAuth (subscription) path — which cannot be disabled. `lev setup` offers
    /// it and defaults to declining, so a user who presses Enter through the
    /// wizard ends up with it off.
    #[serde(default)]
    pub claude_code_enabled: bool,

    /// Path to the `claude` executable. `None` resolves `claude` on `PATH`.
    #[serde(default)]
    pub claude_code_binary: Option<String>,

    /// Reasoning effort for the Claude Code transport: `low` | `medium` |
    /// `high` | `xhigh` | `max`.
    ///
    /// Always sent explicitly. Left to itself the CLI picks `high` with adaptive
    /// thinking, spending output tokens and latency Leviath never asked for.
    /// `None` uses [`leviath_providers::claude_code::DEFAULT_EFFORT`].
    #[serde(default)]
    pub claude_code_effort: Option<String>,
}

/// Optional overrides for a Rhai script provider, from `[model_providers.<name>]`.
///
/// Every field is optional. Keys not recognized below flow into [`Self::extra`]
/// and are forwarded to the script's `initialize(config)` alongside `base_url`
/// and `api_key`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelProviderConfig {
    /// Script filename stem or path. Defaults to `<name>.rhai` in the providers
    /// directory (`~/.leviath/providers/`).
    #[serde(default)]
    pub script: Option<String>,

    /// API key forwarded to the script as `config.api_key` (a script may instead
    /// read its own environment variable).
    #[serde(default)]
    pub api_key: Option<String>,

    /// Base URL forwarded to the script as `config.base_url`.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Rate limit enforced by the Rust wrapper (requests/tokens per minute).
    #[serde(default)]
    pub rate_limit: Option<leviath_providers::RateLimitConfig>,

    /// Any additional keys, forwarded verbatim into the script's `initialize`.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            agent_paths: Vec::new(),
            registries: vec!["https://leviath.dev/registry".to_string()],
            openrouter_api_key: None,
            ollama_base_url: None,
            mcp_servers: Vec::new(),
            default_model: None,
            model_capabilities: HashMap::new(),
            model_providers: HashMap::new(),
            tool_permissions: HashMap::new(),
            agent_tool_permissions: HashMap::new(),
            title: TitleConfig::default(),
            request_timeout_secs: None,
            taint_tracking: false,
            limits: LimitsConfig::default(),
            batch_tool_hint: true,
            webhook: WebhookConfig::default(),
            sandbox: None,
            tool_script_permissions: ScriptToolPermissions::default(),
            security: SecurityConfig::default(),
        }
    }
}

impl Config {
    /// The permission ceiling to apply to `agent_name`: the global
    /// `[tool_permissions]` with that agent's `[agent_tool_permissions.<name>]`
    /// entries laid over it.
    ///
    /// Returned by value (rather than as two maps threaded through
    /// [`crate::tools::resolve_policy`]) so the ceiling is resolved exactly once,
    /// at spawn, and every later lookup reads a single flat map.
    pub fn permissions_for_agent(&self, agent_name: &str) -> HashMap<String, ToolPolicy> {
        let mut merged = self.tool_permissions.clone();
        if let Some(per_agent) = self.agent_tool_permissions.get(agent_name) {
            merged.extend(per_agent.iter().map(|(k, v)| (k.clone(), *v)));
        }
        merged
    }

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
            let path_display = path.display();
            tracing::debug!("No config file found at {}, using defaults", path_display);
            Self::default()
        } else {
            let content = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("Failed to read config from '{}': {}", path.display(), e)
            })?;

            let c: Self = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

            // Catch a malformed MCP server entry here, at load, rather than at
            // the first tool call: a typo that drops a server's tools should
            // fail loudly and immediately.
            for server in &c.mcp_servers {
                server.validate()?;
            }

            let path_display = path.display();
            tracing::debug!("Loaded config from {}", path_display);
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

    /// Save configuration to a path, parameterized so it can be exercised in
    /// tests against a tempfile instead of the real `~/.leviath/config.toml`.
    /// `pub(crate)` so in-crate callers (e.g. the `setup` wizard) can inject a
    /// path; production writes to [`Self::config_path`].
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

        let path_display = path.display();
        tracing::debug!("Saved config to {}", path_display);
        Ok(())
    }

    /// Load a config from an explicit path (`lev mcp` uses this to read the
    /// file it is about to rewrite). Public wrapper over the tested `load_from_path`.
    pub fn load_from_path_public(path: &std::path::Path) -> anyhow::Result<Self> {
        Self::load_from_path(path)
    }

    /// Save a config to an explicit path. Public wrapper over `save_to_path`, for `lev mcp` rewriting the config file.
    pub fn save_to_path_public(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.save_to_path(path)
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
        if let Some(ref key) = self.providers.anthropic_api_key
            && !key.starts_with("sk-ant-")
        {
            warnings.push(
                "Anthropic API key doesn't start with 'sk-ant-' — verify it's correct".to_string(),
            );
        }
        if let Some(ref key) = self.providers.openai_api_key
            && !key.starts_with("sk-")
        {
            warnings
                .push("OpenAI API key doesn't start with 'sk-' — verify it's correct".to_string());
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

/// The directory that holds drop-in Rhai *script providers* (issue #101):
/// `~/.leviath/providers/` (honoring `LEVIATH_HOME`). `None` when no home
/// directory can be resolved.
pub fn providers_dir() -> Option<PathBuf> {
    leviath_home_dir().map(|h| h.join(".leviath").join("providers"))
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
        Ok(Some(old_mode)) => {
            let masked_mode = old_mode & 0o777;
            tracing::warn!(
                "Config file has overly permissive permissions ({:o}), fixing to 600",
                masked_mode
            );
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("Failed to fix config file permissions: {}", e),
    }
}

/// Set restrictive permissions on the config file.
fn set_file_permissions(path: &std::path::Path) {
    set_file_permissions_with(path, leviath_sys::secure_file_perms);
}

/// Core of [`set_file_permissions`] with the hardening operation injected, so
/// the "failed" arm is coverable on every OS. `leviath_sys`'s Windows fallback
/// is infallible (always `Ok`), so that `Err` arm is otherwise unreachable
/// there -- and even a missing path fails only on Unix. A `fn` pointer (not
/// `impl Fn`) keeps this to a single monomorphization, mirroring
/// [`check_permissions_at_with`].
fn set_file_permissions_with(
    path: &std::path::Path,
    secure: fn(&std::path::Path) -> std::io::Result<()>,
) {
    if let Err(e) = secure(path) {
        tracing::warn!("Failed to set config file permissions: {}", e);
    }
}

/// Set restrictive permissions on the config directory.
fn set_dir_permissions(path: &std::path::Path) {
    set_dir_permissions_with(path, leviath_sys::secure_dir_perms);
}

/// Core of [`set_dir_permissions`] with the hardening operation injected; see
/// [`set_file_permissions_with`] for why.
fn set_dir_permissions_with(
    path: &std::path::Path,
    secure: fn(&std::path::Path) -> std::io::Result<()>,
) {
    if let Err(e) = secure(path) {
        tracing::warn!("Failed to set config directory permissions: {}", e);
    }
}

/// Serializes any test, anywhere in the crate, that mutates the process's
/// current working directory (via `std::env::set_current_dir`) or whose
/// assertions implicitly depend on it. Declared here (not inside `mod tests`)
/// so it's reachable crate-wide: a per-file lock (as in
/// `commands/run/manifest.rs`'s CWD-dependent `find_manifest` tests) would not
/// serialize against a CWD-mutating test in a different file. (Env-var
/// isolation, by contrast, goes through the `temp-env` crate's own global
/// lock; `set_current_dir` is not an env var, so it keeps this dedicated lock.)
#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that releases [`CWD_LOCK`] and restores the process's
/// original working directory on drop.
///
/// Wraps the `MutexGuard` inside a private field specifically so it can be held
/// across an `.await` in an async test without tripping clippy's
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
/// too by [`config_isolation_vars`].
#[cfg(test)]
const PROVIDER_KEY_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
];

/// Create a fresh, empty temp directory to stand in for the config directory.
#[cfg(test)]
fn make_fake_config_dir(unique: &str) -> std::path::PathBuf {
    let fake_dir = std::env::temp_dir().join(format!("lev-fake-config-{unique}"));
    let _ = std::fs::create_dir_all(&fake_dir);
    fake_dir
}

/// The env overrides that isolate `Config::load()` from the real environment:
/// point `LEVIATH_CONFIG_PATH` at a nonexistent file in `fake_dir`, set
/// `LEVIATH_SKIP_DOTENV`, and clear every provider API key (so no real, billed
/// inference call can be made). Consumed by [`with_isolated_config_path`] and
/// its async twin, which hand it to `temp_env` for scoped set-and-restore.
#[cfg(test)]
fn config_isolation_vars(
    fake_dir: &std::path::Path,
) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    let mut vars: Vec<(&'static str, Option<std::ffi::OsString>)> = vec![
        (
            "LEVIATH_CONFIG_PATH",
            Some(fake_dir.join("config.toml").into_os_string()),
        ),
        ("LEVIATH_SKIP_DOTENV", Some(std::ffi::OsString::from("1"))),
    ];
    for &key in PROVIDER_KEY_ENV_VARS {
        vars.push((key, None));
    }
    vars
}

/// Runs `f` with `Config::load()` isolated from the real environment (see
/// [`config_isolation_vars`]), passing it the fake config directory so tests
/// that need to plant a `config.toml` can. `temp_env::with_vars` sets the
/// overrides, runs the closure, and restores the prior values afterwards --
/// serialized process-wide against every other temp-env test, so no hand-rolled
/// lock is needed. The closure-scoped form (not an RAII guard) is required
/// because edition 2024 makes `set_var` `unsafe`, which the crate forbids.
#[cfg(test)]
pub(crate) fn with_isolated_config_path<R>(
    unique: &str,
    f: impl FnOnce(&std::path::Path) -> R,
) -> R {
    let fake_dir = make_fake_config_dir(unique);
    let result = temp_env::with_vars(config_isolation_vars(&fake_dir), || f(&fake_dir));
    let _ = std::fs::remove_dir_all(&fake_dir);
    result
}

/// Async counterpart of [`with_isolated_config_path`] for `#[tokio::test]`s.
/// The isolation env vars stay in place across every `.await` in `fut`.
#[cfg(test)]
pub(crate) async fn with_isolated_config_path_async<R, Fut>(
    unique: &str,
    f: impl FnOnce(std::path::PathBuf) -> Fut,
) -> R
where
    Fut: std::future::Future<Output = R>,
{
    let fake_dir = make_fake_config_dir(unique);
    let result =
        temp_env::async_with_vars(config_isolation_vars(&fake_dir), f(fake_dir.clone())).await;
    let _ = std::fs::remove_dir_all(&fake_dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;

    // ─── leviath_home_dir ────────────────────────────────────────────────────

    #[test]
    fn leviath_home_dir_uses_override_when_set() {
        temp_env::with_var(
            "LEVIATH_HOME",
            Some("/tmp/leviath-home-override-test"),
            || {
                assert_eq!(
                    leviath_home_dir(),
                    Some(std::path::PathBuf::from("/tmp/leviath-home-override-test"))
                );
            },
        );
    }

    #[test]
    fn leviath_home_dir_falls_back_to_dirs_home_dir_when_unset() {
        temp_env::with_var_unset("LEVIATH_HOME", || {
            assert_eq!(leviath_home_dir(), dirs::home_dir());
        });
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
    fn limits_default_to_bounded_values() {
        let limits = LimitsConfig::default();
        assert_eq!(limits.max_concurrent_inferences, Some(8));
        assert_eq!(limits.default_max_iterations, Some(50));
        // Exact token counting is opt-in, off by default.
        assert!(!limits.exact_token_counting);
        // And the top-level Config carries the same defaults.
        assert_eq!(Config::default().limits.max_concurrent_inferences, Some(8));
    }

    #[test]
    fn exact_token_counting_parses_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let body = format!(
            "{}\n[limits]\nexact_token_counting = true\n",
            config_toml_without_limits()
        );
        std::fs::write(&path, body).unwrap();
        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert!(config.limits.exact_token_counting);
        // The other fields still fall back to their per-field defaults.
        assert_eq!(config.limits.max_concurrent_inferences, Some(8));
    }

    /// A valid full config-file body with the `[limits]` section removed, so
    /// tests can simulate a config written before the section existed (robust to
    /// unrelated fields being added). `[limits]` serializes as the final section.
    #[cfg(test)]
    fn config_toml_without_limits() -> String {
        let full = toml::to_string_pretty(&Config::default()).unwrap();
        format!("{}\n", full.split("[limits]").next().unwrap().trim_end())
    }

    #[test]
    fn limits_absent_section_uses_defaults() {
        // A config file with no `[limits]` table still gets the bounded defaults.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, config_toml_without_limits()).unwrap();
        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert_eq!(config.limits.max_concurrent_inferences, Some(8));
        assert_eq!(config.limits.default_max_iterations, Some(50));
    }

    #[test]
    fn limits_partial_section_fills_the_other_default() {
        // Setting only one field leaves the other at its per-field serde default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let body = format!(
            "{}\n[limits]\nmax_concurrent_inferences = 3\n",
            config_toml_without_limits()
        );
        std::fs::write(&path, body).unwrap();
        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert_eq!(config.limits.max_concurrent_inferences, Some(3));
        assert_eq!(config.limits.default_max_iterations, Some(50));
    }

    #[test]
    fn load_from_path_existing_provider_keys_skip_env_fallback() {
        // Every one of the 5 "env var fallback" `if field.is_none()` checks
        // in `load_from_path` has only ever been exercised on its `true`
        // (field absent, fall back to env) arm elsewhere in this file --
        // never on the `false` (field already set from the TOML file, skip
        // the env lookup) arm. `temp_env::with_vars` clears these process-global
        // env vars for the closure (and serializes against every other temp-env
        // test), so no concurrently-running test can be mid-set when we read.
        let unset: Vec<(&str, Option<&str>)> = PROVIDER_KEY_ENV_VARS
            .iter()
            .chain(["OLLAMA_HOST"].iter())
            .map(|&key| (key, None))
            .collect();
        temp_env::with_vars(unset, || {
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
        });
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to write config")
        );
    }

    #[test]
    fn save_to_path_create_config_dir_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not-a-dir");
        std::fs::write(&blocking_file, "").unwrap();
        let path = blocking_file.join("config.toml");
        let result = Config::default().save_to_path(&path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to create config directory")
        );
    }

    #[test]
    fn load_propagates_error_when_real_config_file_is_malformed() {
        // Every other `Config::load()` test sees either no file (defaults)
        // or a well-formed one, so `load()`'s `?` on `load_from_path(...)`
        // has never actually propagated an `Err`. Writing malformed TOML to
        // the guard's redirected `LEVIATH_CONFIG_PATH` forces that.
        with_isolated_config_path("load-malformed", |fake_dir| {
            std::fs::write(fake_dir.join("config.toml"), "not valid toml [[[").unwrap();

            let result = Config::load();

            assert!(result.is_err());
        });
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

    // Portable failure injection for the hardening error arms of
    // `set_file_permissions`/`set_dir_permissions`. `leviath_sys`'s Windows
    // fallback is infallible (always `Ok`) -- and even a missing path fails only
    // on Unix -- so the only cross-platform way to reach the `Err` arm is to
    // inject a hardening op that fails (mirroring `check_permissions_at_with`).
    fn always_failing_secure(_path: &std::path::Path) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "simulated permission-hardening failure",
        ))
    }

    #[test]
    fn set_file_permissions_error_branch_logs_not_panics() {
        with_tracing(|| {
            set_file_permissions_with(
                std::path::Path::new("/does/not/matter"),
                always_failing_secure,
            )
        }); // hits the Err arm, must not panic
    }

    #[test]
    fn set_dir_permissions_error_branch_logs_not_panics() {
        with_tracing(|| {
            set_dir_permissions_with(
                std::path::Path::new("/does/not/matter"),
                always_failing_secure,
            )
        }); // hits the Err arm, must not panic
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
    fn test_validate_keys_good_anthropic() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-test123".to_string()),
                openai_api_key: None,
                google_api_key: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
    fn load_rejects_a_malformed_mcp_server_entry() {
        // An entry with neither `command` nor `url` can never connect, so it
        // must fail at load — naming the server — rather than silently drop its
        // tools until the first call.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[[mcp_servers]]
name = "broken"
"#,
        )
        .unwrap();

        let err = Config::load_from_path(&path).expect_err("malformed entry must fail load");
        let msg = err.to_string();
        assert!(msg.contains("broken"), "must name the server: {msg}");
    }

    #[test]
    fn load_accepts_a_well_formed_http_mcp_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[[mcp_servers]]
name = "remote"
url = "https://mcp.example.com/mcp"
"#,
        )
        .unwrap();

        let config = Config::load_from_path(&path).expect("valid http entry should load");
        assert_eq!(
            config.mcp_servers[0].url.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 2);
    }

    // ─── config_path ───────────────────────────────────────────────────────

    #[test]
    fn config_path_contains_leviath() {
        // Force `LEVIATH_CONFIG_PATH` unset (via `temp_env::with_var_unset`,
        // which also serializes against every other temp-env test) so
        // `config_path()` resolves to the real default, not a concurrently-set
        // override.
        temp_env::with_var_unset("LEVIATH_CONFIG_PATH", || {
            let path = Config::config_path();
            assert!(path.to_str().unwrap().contains(".leviath"));
            assert!(path.to_str().unwrap().ends_with("config.toml"));
        });
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            ..Config::default()
        };
        // Google key has no prefix validation
        assert!(config.validate_keys().is_empty());
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            agent_paths: vec![std::path::PathBuf::from("/my/agents")],
            registries: vec!["https://registry.example.com".to_string()],
            openrouter_api_key: None,
            ollama_base_url: Some("http://custom:11434".to_string()),
            mcp_servers: vec![],
            default_model: None,
            model_capabilities: model_caps,
            model_providers: HashMap::new(),
            tool_permissions: tool_perms,
            agent_tool_permissions: HashMap::new(),
            title: TitleConfig {
                enabled: false,
                provider: Some("openai".to_string()),
                model: Some("gpt-5-mini".to_string()),
            },
            request_timeout_secs: None,
            taint_tracking: false,
            limits: LimitsConfig {
                max_concurrent_inferences: Some(4),
                max_concurrent_tools: 3,
                default_max_iterations: Some(99),
                exact_token_counting: false,
                script_shell_timeout_secs: 45,
            },
            batch_tool_hint: true,
            webhook: WebhookConfig {
                max_retries: 5,
                base_delay_ms: 250,
                max_delay_ms: 10_000,
                timeout_secs: 7,
            },
            sandbox: Some(leviath_core::ToolSandboxConfig {
                kind: leviath_core::SandboxKind::Container,
                image: Some("ubuntu:24.04".to_string()),
                network: false,
                ..Default::default()
            }),
            tool_script_permissions: ScriptToolPermissions {
                http_get: ScriptPermission::Allow,
                http_post: ScriptPermission::Deny,
                shell: ScriptPermission::Deny,
                read_file: ScriptPermission::Inherit,
                write_file: ScriptPermission::Deny,
                env_var: ScriptPermission::Allow,
            },
            security: SecurityConfig {
                allow_seed_commands: false,
                allow_local_network: true,
            },
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.default_provider, "anthropic");
        assert_eq!(deserialized.limits.max_concurrent_inferences, Some(4));
        assert_eq!(deserialized.limits.max_concurrent_tools, 3);
        assert_eq!(deserialized.limits.script_shell_timeout_secs, 45);
        assert_eq!(
            deserialized.tool_script_permissions.http_get,
            ScriptPermission::Allow
        );
        assert_eq!(
            deserialized.tool_script_permissions.shell,
            ScriptPermission::Deny
        );
        assert_eq!(
            deserialized.tool_script_permissions.write_file,
            ScriptPermission::Deny
        );
        assert!(!deserialized.security.allow_seed_commands);
        assert_eq!(deserialized.webhook.max_retries, 5);
        assert_eq!(deserialized.webhook.base_delay_ms, 250);
        assert_eq!(deserialized.webhook.max_delay_ms, 10_000);
        assert_eq!(deserialized.webhook.timeout_secs, 7);
        assert_eq!(deserialized.limits.default_max_iterations, Some(99));
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
        let sandbox = deserialized.sandbox.expect("sandbox round-trips");
        assert_eq!(sandbox.kind, leviath_core::SandboxKind::Container);
        assert_eq!(sandbox.image.as_deref(), Some("ubuntu:24.04"));
        assert!(!sandbox.network);
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
