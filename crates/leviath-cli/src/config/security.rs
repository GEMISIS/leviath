//! `[security]`: what an agent's own code and commands are allowed to reach.
//!
//! The section that decides whether a blueprint may loosen a policy, which
//! environment variables a shell or script can see, and where `[read_paths]`
//! grants apply. Its defaults are the shipped posture, so a change here is a
//! change to what a fresh install permits.

use serde::{Deserialize, Serialize};

use super::default_true;

/// One agent's entry in `[agent_read_paths.<name>]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadPathGrants {
    /// Granted entries, same forms as a blueprint's `[read_paths] allow`.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// `[security]` in `~/.leviath/config.toml`.
///
/// Distinct from a *blueprint's* `[security]` block, which configures taint
/// tracking for one agent - this one holds machine-wide switches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityConfig {
    /// Directories a run's workdir may sit under without being confirmed.
    ///
    /// **Empty by default**, which means "ask once about anything alarming"
    /// rather than "ask about everything": a workdir is only questioned when it
    /// is a home directory or a filesystem root, which is where an agent's file
    /// writes do the most damage and the least good. Listing a path here says
    /// "yes, I work there" and silences the prompt for it and everything under
    /// it.
    ///
    /// This exists because the workdir defaults to wherever `lev run` was
    /// invoked, and running from `~` is an easy thing to do by accident - issue
    /// #252 is a machine that lost 115 GB to an agent writing under a profile
    /// root. Confirming is cheap; noticing afterwards is not.
    #[serde(default)]
    pub allowed_workdirs: Vec<String>,

    /// Whether a blueprint's `seed = { command = "..." }` regions may run.
    ///
    /// **On by default.** A command seed executes at spawn - before the first
    /// inference, and therefore before any tool-approval prompt - so it is the
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
    /// of context an attacker can influence - a search result, a page fetched a
    /// moment ago, an issue body - so an unrestricted fetch makes the agent a
    /// confused deputy *inside* the user's network. The concrete targets are
    /// `http://169.254.169.254/…` (cloud metadata, which returns instance
    /// credentials), `http://127.0.0.1:3000/api/…` (the user's own `lev serve`),
    /// and anything on the LAN.
    ///
    /// Turn this on when the agent is genuinely meant to talk to something local -
    /// a self-hosted model, a dev server under test. It applies to the script
    /// host's `http_get`/`http_post` and to redirect following; see
    /// [`leviath_core::net`].
    #[serde(default)]
    pub allow_local_network: bool,

    /// Credential-shaped environment variables that agent scripts may read.
    ///
    /// A Rhai script tool or script provider calling `env_var("NAME")` gets any
    /// ordinary variable - `PATH`, `TZ`, an app's own config. A name that *looks
    /// like a credential* (see [`leviath_core::secrets::is_sensitive_env_name`])
    /// is refused unless it appears here, because a two-line script tool reading
    /// `ANTHROPIC_API_KEY` and POSTing it elsewhere was otherwise a working
    /// exfiltration path with no prompt anywhere in it.
    ///
    /// List the exact names a script legitimately needs - typically the key for
    /// a custom provider script:
    ///
    /// ```toml
    /// [security]
    /// allow_env_vars = ["MY_PROVIDER_KEY"]
    /// ```
    ///
    /// Matching is case-insensitive and exact. There is no wildcard: `"*"` is
    /// read as a variable literally named `*`, not as "allow everything".
    #[serde(default)]
    pub allow_env_vars: Vec<String>,

    /// How much of the daemon's environment a `shell` tool call, a Rhai
    /// `shell()` host call, and a region's command seed inherit.
    ///
    /// The daemon holds provider keys, `LEVIATH_API_TOKEN`, and whatever
    /// credentials the person who started it had exported. Handing all of that
    /// to every shell command means a single `env` in tool output leaks the lot.
    ///
    /// `filtered` (the default) withholds credential-shaped names but keeps
    /// `SSH_AUTH_SOCK`, so `git push` over agent keys still works. `strict`
    /// drops the carve-out and also takes `AWS_PROFILE`, `KUBECONFIG` and
    /// friends. `custom` ignores the shape heuristic and withholds exactly what
    /// [`Self::shell_env_withhold`] names. `inherit` is the old behaviour.
    ///
    /// Toolchain variables - `PATH`, `HOME`, `CARGO_HOME`, `JAVA_HOME`,
    /// `VIRTUAL_ENV`, `NVM_DIR`, `GOPATH`, `DOCKER_HOST` - pass through under
    /// every mode. [`Self::allow_env_vars`] hands a specific name over under
    /// every mode too.
    #[serde(default)]
    pub shell_env: leviath_core::ShellEnvMode,

    /// The names `shell_env = "custom"` withholds. Ignored under every other
    /// mode, where the name-shape heuristic decides instead.
    #[serde(default)]
    pub shell_env_withhold: Vec<String>,

    /// Whether a blueprint's `[read_paths]` declarations are honored as-is.
    ///
    /// **Off by default.** A `[read_paths]` block travels inside the
    /// `agent.leviath` you installed, and a manifest may only *tighten* what
    /// your config allows, never widen it - otherwise any agent package could
    /// read `~/.ssh`, this very config file (your API keys), or a password
    /// store by shipping one TOML line. With this off, an agent's declared
    /// read paths are inert until you grant them via [`Self::read_paths`] or
    /// `[agent_read_paths.<name>]`. Turning it on says "any blueprint I run
    /// may read every path it declares" - reads only, each access still
    /// resolves symlinks and must land inside a declared entry, but prefer
    /// the per-agent grant for anything you did not author yourself.
    #[serde(default)]
    pub allow_blueprint_read_paths: bool,

    /// Honour every blueprint's own `[safe_commands]` block.
    ///
    /// **Off by default**, and for the same reason as
    /// [`Self::allow_blueprint_read_paths`]: a `[safe_commands]` block travels
    /// inside an `agent.leviath` you installed, so letting it count by itself
    /// would let any agent package pre-approve its own shell with one TOML line.
    /// With this off, a blueprint's list is inert until you opt in, either here
    /// for every agent or per agent via
    /// `[agent_safe_commands.<name>] allow_blueprint = true`. Prefer the
    /// per-agent grant for anything you did not author yourself.
    #[serde(default)]
    pub allow_blueprint_safe_commands: bool,

    /// Honour a blueprint's `[tool_permissions]` even where it is *more*
    /// permissive than the built-in default for a tool you have not configured.
    ///
    /// Off by default, for the same reason as the two switches around it:
    /// declaring is not granting. Saying nothing about `shell` is the normal
    /// state, so without this a downloaded manifest could give itself
    /// `shell = "allow"` on a stock machine. With it off, a blueprint may still
    /// pre-approve the read-only web tools that are some agents' whole point,
    /// and anything beyond that is clamped to the built-in default.
    ///
    /// The per-agent grant needs no switch of its own: naming the tool under
    /// `[agent_tool_permissions.<name>]` makes it a ceiling for that agent, and
    /// a blueprint may go up to a ceiling. Prefer that for anything you did not
    /// author yourself - it says which agent and which tool, where this says
    /// "every agent, every tool".
    #[serde(default)]
    pub allow_blueprint_permissions: bool,

    /// Machine-wide read grants for agents that declare `[read_paths]`.
    ///
    /// Entries use the same three forms as a blueprint's `[read_paths] allow`:
    /// an exact path (grants its subtree), `glob:` and `regex:` patterns
    /// (matched against the symlink-resolved real path, written with `/` on
    /// every OS, regex auto-anchored). `~/` expands to your home; a relative
    /// entry resolves against the run's workdir.
    ///
    /// ```toml
    /// [security]
    /// read_paths = ["~/.leviath/runs", "glob:~/design-docs/**"]
    /// ```
    ///
    /// A grant only takes effect for a path the running blueprint *also*
    /// declares - by itself it grants nothing, so listing a directory here
    /// does not open it to agents that never asked.
    #[serde(default)]
    pub read_paths: Vec<String>,

    /// Where provider API keys and MCP OAuth tokens are kept.
    ///
    /// **`file` by default** - `~/.leviath/config.toml` and
    /// `~/.leviath/mcp-auth.json`, both created `0600` so they are never even
    /// briefly world-readable. This is what Claude Code and Codex do, and it is
    /// the only backend that works headless, in a container, over SSH, and on a
    /// CI runner.
    ///
    /// Set it to `keychain` to move secrets into the OS credential store (macOS
    /// Keychain, Windows Credential Manager, Secret Service elsewhere), so a
    /// stolen `~/.leviath` directory yields nothing:
    ///
    /// ```toml
    /// [security]
    /// credential_store = "keychain"
    /// ```
    ///
    /// Then run `lev auth migrate` to move the secrets you already have. It is
    /// opt-in rather than the default because an unavailable keychain is not a
    /// degraded experience but a broken one - every inference fails at once -
    /// and the environments Leviath is most useful in are the least likely to
    /// have a working credential store. `lev auth status` reports whether this
    /// machine actually has one.
    #[serde(default)]
    pub credential_store: leviath_core::CredentialStoreKind,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            allowed_workdirs: Vec::new(),
            allow_seed_commands: true,
            allow_local_network: false,
            allow_env_vars: Vec::new(),
            shell_env: leviath_core::ShellEnvMode::default(),
            shell_env_withhold: Vec::new(),
            allow_blueprint_read_paths: false,
            allow_blueprint_safe_commands: false,
            allow_blueprint_permissions: false,
            read_paths: Vec::new(),
            credential_store: leviath_core::CredentialStoreKind::File,
        }
    }
}
