//! The daemon's real [`ScriptHost`] for Rhai script tools (permission Layer 3).
//!
//! A registered script tool reaches the outside world only through the host
//! functions on [`leviath_scripting::ScriptHost`]. This module supplies the real
//! implementation: it enforces the per-function `[tool_script_permissions]`
//! (allow / deny / inherit) resolved at agent spawn, confines `read_file` /
//! `write_file` to the agent workdir, routes `shell()` through the agent's
//! per-stage sandbox with a wall-clock timeout, and performs the actual I/O.
//!
//! The I/O itself lives behind the [`ScriptIo`] seam so the permission and
//! path-confinement logic is unit-testable with a fake, and the real
//! network/process/filesystem/env behavior ([`RealScriptIo`]) is exercised with
//! hermetic, local resources (a mock HTTP server, `echo`, temp files, scoped env
//! vars) - the same approach the MCP and package-registry tests use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leviath_core::floor_char_boundary;
use leviath_scripting::ScriptHost;
use leviath_tools::ShellExecutor;
use tokio::process::Command as TokioCommand;

use crate::config::{ScriptPermission, ScriptToolPermissions, ToolPolicy};
use crate::daemon::sandbox_manager::SandboxManager;

/// The resolved allow/deny decision for each of the five side-effecting host
/// functions, computed once at spawn from the config's `[tool_script_permissions]`
/// and the agent's own tool permissions (for the `inherit` cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptAllow {
    /// Whether `http_get` may run.
    pub http_get: bool,
    /// Whether `http_post` may run.
    pub http_post: bool,
    /// Whether `shell` may run.
    pub shell: bool,
    /// Whether `read_file` may run.
    pub read_file: bool,
    /// Whether `write_file` may run.
    pub write_file: bool,
    /// Whether `env_var` may run.
    pub env_var: bool,
}

/// Resolve `[tool_script_permissions]` into concrete allow/deny booleans.
///
/// `Allow`/`Deny` map directly. `Inherit` means:
/// - `read_file` / `write_file` / `shell`: permitted only when the agent's resolved policy for
///   the equivalent built-in (`resolve_builtin`) is [`ToolPolicy::Allow`]. This
///   is evaluated once against the entry stage's permission layers; a later
///   stage's `tool_permissions` do not re-gate a script's host calls.
/// - `http_get` / `http_post` / `env_var`: permitted (no built-in equivalent to
///   inherit from, and the tool itself is still gated by Layers 1/2/4).
///
/// `resolve_builtin` is a `&dyn Fn` (not `impl Fn`) so this function has a single
/// monomorphization; otherwise each distinct caller closure type gets its own
/// copy of the `net`/`filelike` match arms, and coverage is attributed
/// per-instantiation (each only exercises the arms that caller hits).
pub fn resolve_script_permissions(
    perms: &ScriptToolPermissions,
    resolve_builtin: &dyn Fn(&str) -> ToolPolicy,
) -> ScriptAllow {
    let net = |p: ScriptPermission| match p {
        ScriptPermission::Allow | ScriptPermission::Inherit => true,
        ScriptPermission::Deny => false,
    };
    let filelike = |p: ScriptPermission, builtin: &str| match p {
        ScriptPermission::Allow => true,
        ScriptPermission::Deny => false,
        ScriptPermission::Inherit => resolve_builtin(builtin) == ToolPolicy::Allow,
    };
    ScriptAllow {
        http_get: net(perms.http_get),
        http_post: net(perms.http_post),
        env_var: net(perms.env_var),
        read_file: filelike(perms.read_file, "read_file"),
        write_file: filelike(perms.write_file, "write_file"),
        shell: filelike(perms.shell, "shell"),
    }
}

/// Map a `[tool_script_permissions]` string to a [`ScriptPermission`]. An
/// unrecognized value yields `None` (the field is left at the global default) -
/// parsed by hand (not via `Deserialize`) so every arm is deterministically
/// covered, without pulling in serde's unexercised visitor machinery.
fn parse_script_permission_str(s: &str) -> Option<ScriptPermission> {
    match s {
        "allow" => Some(ScriptPermission::Allow),
        "deny" => Some(ScriptPermission::Deny),
        "inherit" => Some(ScriptPermission::Inherit),
        _ => None,
    }
}

/// How restrictive a script permission is, for clamping.
///
/// `Allow` (unconditional) is the loosest; `Inherit` still requires the agent's
/// own policy for the equivalent built-in to permit the call; `Deny` is the
/// tightest.
fn script_restrictiveness(p: ScriptPermission) -> u8 {
    match p {
        ScriptPermission::Allow => 0,
        ScriptPermission::Inherit => 1,
        ScriptPermission::Deny => 2,
    }
}

/// The effective `[tool_script_permissions]` for an agent: the user's global
/// config with the agent's own blueprint `[tool_script_permissions]` overlaid
/// per field - but **only where the manifest is more restrictive**.
///
/// Agents ship their own `.rhai` tool scripts, so it is reasonable for a
/// manifest to say "this agent never needs `shell`". It is not reasonable for it
/// to say the opposite: a manifest that could set `shell = "allow"` over a user's
/// global `deny` meant installing an agent was enough to overrule the machine's
/// configuration. So a manifest may tighten a field and never loosen it, the same
/// rule [`crate::tools::resolve_policy`] applies to `[tool_permissions]`.
///
/// Parsed CLI-side (these types live in the CLI config, not `leviath-core`),
/// mirroring `parse_blueprint_mcp_servers`.
pub fn effective_script_permissions(
    global: &ScriptToolPermissions,
    manifest_toml: &str,
) -> ScriptToolPermissions {
    let mut eff = global.clone();
    // `toml::from_str`, not `manifest_toml.parse::<toml::Value>()`. In toml 1.x
    // `FromStr for Value` parses a single *value*, not a document - so a real
    // manifest starting with `[agent]` reads as an array literal followed by
    // junk and fails. It still compiles, so the change is silent; the tests are
    // what caught it.
    let Ok(value) = toml::from_str::<toml::Value>(manifest_toml) else {
        return eff;
    };
    let Some(table) = value
        .get("tool_script_permissions")
        .and_then(|v| v.as_table())
    else {
        return eff;
    };
    // For each key the agent set to a recognized value, keep whichever of the
    // two is stricter.
    let apply = |key: &str, slot: &mut ScriptPermission| {
        if let Some(p) = table
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(parse_script_permission_str)
            && script_restrictiveness(p) > script_restrictiveness(*slot)
        {
            *slot = p;
        }
    };
    apply("http_get", &mut eff.http_get);
    apply("http_post", &mut eff.http_post);
    apply("shell", &mut eff.shell);
    apply("read_file", &mut eff.read_file);
    apply("write_file", &mut eff.write_file);
    apply("env_var", &mut eff.env_var);
    eff
}

/// The raw I/O a [`DaemonScriptHost`] performs, behind a seam so the host's
/// permission/confinement logic is testable without real side effects.
pub trait ScriptIo: Send + Sync {
    /// Perform an HTTP GET, returning the response body (or an error message).
    fn http_get(&self, url: &str, headers: BTreeMap<String, String>) -> Result<String, String>;
    /// Perform an HTTP POST, returning the response body (or an error message).
    fn http_post(
        &self,
        url: &str,
        body: &str,
        headers: BTreeMap<String, String>,
    ) -> Result<String, String>;
    /// Run a prepared shell command (already sandbox-wrapped and pointed at the
    /// workdir by the host), enforcing `timeout`, and return its combined output.
    fn run_shell(&self, cmd: TokioCommand, timeout: Duration) -> Result<String, String>;
    /// Read the file at an already-confined absolute `path`.
    fn read_file(&self, path: &Path) -> Result<String, String>;
    /// Write `content` to an already-confined absolute `path`, creating parent
    /// directories as needed. Returns a short confirmation.
    fn write_file(&self, path: &Path, content: &str) -> Result<String, String>;
    /// Read environment variable `name`.
    fn env_var(&self, name: &str) -> Result<String, String>;
}

/// The daemon's script host: enforces permissions + workdir confinement, then
/// delegates the actual work to a [`ScriptIo`].
pub struct DaemonScriptHost {
    allow: ScriptAllow,
    workdir: PathBuf,
    io: Arc<dyn ScriptIo>,
    /// The agent's sandbox manager, if any. When present, a script `shell()`
    /// call runs inside the *current* stage's sandbox (container / namespace),
    /// exactly like the built-in `shell` tool - a script can't escape the
    /// isolation the agent's stage declared. `None` runs on the host.
    sandbox: Option<Arc<SandboxManager>>,
    /// Wall-clock cap on a single `shell()` call, so a runaway command can't hang
    /// the agent (mirrors the built-in shell tool's timeout).
    shell_timeout: Duration,
    /// `[security] allow_local_network`: whether this agent's fetches may reach
    /// loopback / private / link-local addresses. Off unless the user turned it
    /// on - see [`check_outbound`].
    allow_local_network: bool,
    /// `[security] allow_env_vars`: credential-shaped environment variables this
    /// agent's scripts may read. Empty by default.
    allow_env_vars: Vec<String>,
    /// `[security] shell_env`: which of the daemon's variables a script's
    /// `shell()` hands to the child. The same policy the built-in shell tool
    /// applies, so `shell()` is not a way around the `env_var` gate.
    shell_env: leviath_tools::ShellEnvPolicy,
    /// The run's write budget, when this host serves a run. A script's
    /// `write_file` and a redirect in its `shell` are writes the run pays
    /// for like any other; without this they were the two that did not.
    writes: Option<Arc<crate::daemon::tool_service::WriteBudget>>,
}

impl DaemonScriptHost {
    /// Build a host with an explicit I/O backend (used by tests). Defaults to no
    /// sandbox and the built-in shell tool's 60-second timeout; override with
    /// [`with_shell`](Self::with_shell).
    pub fn with_io(allow: ScriptAllow, workdir: PathBuf, io: Arc<dyn ScriptIo>) -> Self {
        Self {
            allow,
            workdir,
            io,
            sandbox: None,
            shell_timeout: Duration::from_secs(60),
            allow_local_network: false,
            allow_env_vars: Vec::new(),
            shell_env: leviath_tools::ShellEnvPolicy::default(),
            writes: None,
        }
    }

    /// Charge this run's write budget for what scripts write. Consuming
    /// builder used at spawn.
    pub fn with_write_budget(
        mut self,
        writes: Arc<crate::daemon::tool_service::WriteBudget>,
    ) -> Self {
        self.writes = Some(writes);
        self
    }

    /// Permit fetches to loopback / private / link-local addresses, from
    /// `[security] allow_local_network`. Consuming builder used at spawn.
    pub fn with_local_network(mut self, allow: bool) -> Self {
        self.allow_local_network = allow;
        self
    }

    /// Permit scripts to read these credential-shaped environment variables,
    /// from `[security] allow_env_vars`. Consuming builder used at spawn.
    pub fn with_env_allowlist(mut self, names: Vec<String>) -> Self {
        self.allow_env_vars = names;
        self
    }

    /// Build a host wired to the real network/process/filesystem/env backend.
    pub fn new(allow: ScriptAllow, workdir: PathBuf) -> Self {
        Self::with_io(allow, workdir, Arc::new(RealScriptIo))
    }

    /// Route `shell()` through `sandbox` (the agent's per-stage isolation) and cap
    /// each call at `shell_timeout`. Consuming builder used at spawn.
    pub fn with_shell(
        mut self,
        sandbox: Option<Arc<SandboxManager>>,
        shell_timeout: Duration,
        shell_env: leviath_tools::ShellEnvPolicy,
    ) -> Self {
        self.sandbox = sandbox;
        self.shell_timeout = shell_timeout;
        self.shell_env = shell_env;
        self
    }

    /// Resolve a script-supplied file path against the workdir, rejecting both a
    /// `..` escape and a symlink that leaves the directory.
    ///
    /// The same function the built-in file tools use, so a script's
    /// `write_file` and the agent's `write_file` cannot disagree about what a
    /// path is allowed to be. It used to be a line-for-line copy, error
    /// strings included.
    fn resolve_in_workdir(&self, requested: &str) -> Result<PathBuf, String> {
        leviath_tools::resolve_within(requested, &self.workdir, leviath_core::resolves_within)
            .map_err(|e| e.to_string())
    }
}

/// The standard `[denied]` message for a host function blocked by
/// `[tool_script_permissions]`.
fn denied(func: &str) -> String {
    format!("[denied] script host function '{func}' is denied by tool_script_permissions")
}

/// Check a script-supplied URL against the outbound policy before it is sent.
///
/// The URL came from the model, and the model picked it out of context an
/// attacker can influence - so this is the boundary between "the agent browsing
/// the web" and "the agent probing the user's own network on someone else's
/// behalf". See [`leviath_net`] for what is refused and why.
///
/// Lives on the host (the permission/confinement layer) rather than in
/// [`RealScriptIo`], so a test double is subject to the same rule as the real
/// backend and the check cannot be skipped by swapping the I/O out.
fn check_outbound(url: &str, allow_local: bool) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("[denied] invalid URL '{url}': {e}"))?;
    leviath_net::check_url(&parsed, allow_local).map_err(|e| format!("[denied] {e}"))
}

impl ScriptHost for DaemonScriptHost {
    fn http_get(&self, url: &str, headers: BTreeMap<String, String>) -> Result<String, String> {
        if !self.allow.http_get {
            return Err(denied("http_get"));
        }
        check_outbound(url, self.allow_local_network)?;
        self.io.http_get(url, headers)
    }

    fn http_post(
        &self,
        url: &str,
        body: &str,
        headers: BTreeMap<String, String>,
    ) -> Result<String, String> {
        if !self.allow.http_post {
            return Err(denied("http_post"));
        }
        check_outbound(url, self.allow_local_network)?;
        self.io.http_post(url, body, headers)
    }

    fn shell(&self, command: &str) -> Result<String, String> {
        if !self.allow.shell {
            return Err(denied("shell"));
        }
        // The same clamp `clamp_by_effect` applies to a model's `shell` tool
        // call. Without it this is the hole that clamp exists to close, just
        // reached from a script instead of a tool call: an agent shipping its
        // own `.rhai` tools could write through a redirect while `write_file`
        // was denied. Resolved at spawn like the rest of `allow`, so this is a
        // boolean check rather than a second policy lookup.
        if !self.allow.write_file && crate::shell_keys::writes_a_file(command) {
            return Err(denied("write_file (a shell redirect writes a file)"));
        }
        // And the containment half, which no `allow` lifts: this host's own
        // `write_file` is workdir-confined, so its `shell()` redirects are too.
        if let Some(refusal) = crate::tools::escaping_write_refusal(
            "shell",
            &serde_json::json!({ "command": command }),
            &self.workdir,
        ) {
            return Err(refusal);
        }
        let (shell, flag) = default_shell();
        // With a sandbox, build the command that runs inside the current stage's
        // container / namespace; otherwise run the shell directly on the host
        // (both target the agent workdir). Same routing as the built-in shell tool.
        let mut cmd = match &self.sandbox {
            Some(sb) => sb.build_command(shell, flag, command, &self.workdir),
            None => host_shell_command(shell, flag, command, &self.workdir),
        };
        // Same withholding the built-in shell tool applies. A script that has
        // `shell` would otherwise be the way around the `env_var` gate above.
        self.shell_env.apply(&mut cmd);
        let out = self.io.run_shell(cmd, self.shell_timeout);
        if let Some(writes) = &self.writes {
            // A redirect is only measurable after the fact, as in the tool lane.
            writes.record(crate::tools::measured_write_bytes(
                "shell",
                &serde_json::json!({ "command": command }),
                &self.workdir,
            ));
        }
        out
    }

    fn read_file(&self, path: &str) -> Result<String, String> {
        if !self.allow.read_file {
            return Err(denied("read_file"));
        }
        let resolved = self.resolve_in_workdir(path)?;
        self.io.read_file(&resolved)
    }

    fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        if !self.allow.write_file {
            return Err(denied("write_file"));
        }
        // Same rule as the built-in write tools: never let `create_dir_all`
        // resurrect a workspace that disappeared mid-run (issue #107).
        if !std::fs::metadata(&self.workdir).is_ok_and(|m| m.is_dir()) {
            return Err(format!(
                "workspace '{}' is no longer accessible",
                self.workdir.display()
            ));
        }
        let resolved = self.resolve_in_workdir(path)?;
        let bytes = content.len() as u64;
        if let Some(writes) = &self.writes
            && let Some(refusal) = writes.check(&self.workdir, bytes).refusal()
        {
            return Err(refusal);
        }
        let out = self.io.write_file(&resolved, content);
        if out.is_ok()
            && let Some(writes) = &self.writes
        {
            writes.record(bytes);
        }
        out
    }

    fn env_var(&self, name: &str) -> Result<String, String> {
        if !self.allow.env_var {
            return Err(denied("env_var"));
        }
        // A script tool ships inside the agent bundle, so this call is
        // attacker-authored in exactly the case that matters. Ordinary variables
        // pass; a credential-shaped name needs the user to have listed it. Two
        // lines - `env_var("ANTHROPIC_API_KEY")` then `http_post(...)` - was
        // otherwise a working exfiltration path with no prompt in it anywhere.
        if !leviath_core::script_env_allowed(name, &self.allow_env_vars) {
            return Err(format!(
                "[denied] '{name}' looks like a credential. Add it to `[security] \
                 allow_env_vars` in ~/.leviath/config.toml if this agent is meant \
                 to read it."
            ));
        }
        self.io.env_var(name)
    }
}

/// The real I/O backend: blocking HTTP, host shell, filesystem, and env access.
///
/// Every method runs synchronously (the script engine is driven from a
/// `spawn_blocking` context), so a blocking `reqwest` client and `std::process`
/// are safe here.
pub struct RealScriptIo;

/// The one process-wide blocking HTTP client for script tools.
///
/// Built once, then cloned per request. A `reqwest::blocking::Client` owns a
/// dedicated OS thread running a current-thread tokio runtime, so a
/// build-one-per-request shape spawns (and tears down) a thread plus a runtime
/// plus a TLS root-store load for *every* `http_get` - a researcher agent
/// fanning out over dozens of pages can exhaust thread/FD limits, at which
/// point `build()` fails and the `.expect` panics inside a Rhai native call.
/// One shared client also gives connection reuse across calls.
///
/// The builder can still only fail on TLS-backend init, and that failure is
/// contained: `leviath_scripting`'s native-function guards turn a panic here
/// into an ordinary script error instead of aborting the daemon.
static HTTP_CLIENT: std::sync::LazyLock<reqwest::blocking::Client> =
    std::sync::LazyLock::new(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            // This client pools on purpose (see above), which is the one place
            // in the tree where a *stale* pooled connection is possible at all -
            // the provider client keeps no idle connections. reqwest holds an
            // idle connection for 90 seconds by default, and plenty of servers
            // close theirs sooner; reusing one the far end has already dropped
            // fails a request that never really started. Half the server's
            // usual minute is comfortably inside anyone's window, and a fresh
            // handshake on a fetch that arrives more than 30 seconds after the
            // last one costs nothing anybody can measure.
            .pool_idle_timeout(Duration::from_secs(30))
            // Bound the handshake as well as the whole request: without this a
            // host that accepts the SYN and then does nothing sat here for the
            // full 30 seconds with the per-host permit held, so one bad host
            // could hold up the agent's other fetches.
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(30))
            // Re-check every redirect hop. Validating only the URL the script
            // passed is not enough: a perfectly public page answering `302
            // Location: http://169.254.169.254/` lands on the cloud metadata
            // service just the same, and reqwest follows up to 10 hops by
            // default. `limited(5)` also bounds redirect loops.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many redirects");
                }
                match leviath_net::check_url(attempt.url(), local_network_allowed()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(format!("refused to follow redirect: {e}")),
                }
            }))
            .build()
            .expect("failed to build blocking reqwest client")
    });

/// Flatten an error and its `source` chain into one `": "`-joined line.
///
/// reqwest's own `Display` for a refused redirect is "error following redirect
/// for url (…)" - it never mentions the reason, which for us is the whole point:
/// "refused to follow redirect: private address" and "too many redirects" are
/// different problems with different fixes, and both were reaching the script
/// author as the same opaque sentence.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    leviath_providers::rhai_provider::host::error_chain(e)
}

/// Concurrent script-tool HTTP requests allowed per host; `0` is unbounded.
///
/// A process-wide mirror of `[limits] script_http_max_per_host`, for the same
/// reason [`ALLOW_LOCAL_REDIRECTS`] is one: [`HTTP_CLIENT`] is process-wide and
/// the request path has no handle on the config.
static HTTP_MAX_PER_HOST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(4);

/// Apply `[limits] script_http_max_per_host` for this process.
pub fn set_script_http_max_per_host(max: usize) {
    HTTP_MAX_PER_HOST.store(max, std::sync::atomic::Ordering::Relaxed);
}

/// In-flight script-tool requests per host, and the condvar waiters park on.
static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
static IN_FLIGHT_FREED: std::sync::Condvar = std::sync::Condvar::new();

/// A permit to have one request in flight against `host`, released on drop.
///
/// A condvar rather than a `tokio::sync::Semaphore` because script tools run on
/// `spawn_blocking` threads with no runtime handle to await on.
struct HostPermit(Option<String>);

impl HostPermit {
    /// Take a permit for `url`'s host, blocking while that host is at its cap.
    ///
    /// A URL with no host (and an unbounded cap) takes no permit at all, so the
    /// unlimited setting costs nothing.
    fn acquire(url: &str) -> Self {
        let max = HTTP_MAX_PER_HOST.load(std::sync::atomic::Ordering::Relaxed);
        let Some(host) = host_of(url).filter(|_| max > 0) else {
            return Self(None);
        };
        let mut counts = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while counts.get(&host).copied().unwrap_or(0) >= max {
            counts = IN_FLIGHT_FREED
                .wait(counts)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *counts.entry(host.clone()).or_insert(0) += 1;
        Self(Some(host))
    }
}

impl Drop for HostPermit {
    fn drop(&mut self) {
        let Some(host) = self.0.take() else {
            return;
        };
        let mut counts = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `or_insert(1)` rather than `if let Some(..)`: acquire always inserted
        // the key and this is its only remover, so the missing case cannot
        // happen - and written as a branch it was a region no test could enter.
        let remaining = counts.entry(host.clone()).or_insert(1);
        *remaining = remaining.saturating_sub(1);
        if *remaining == 0 {
            counts.remove(&host);
        }
        drop(counts);
        IN_FLIGHT_FREED.notify_all();
    }
}

/// The host part of `url`, lowercased. `None` when it does not parse.
fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_lowercase)
}

/// Seconds a script tool's HTTP request may take; `0` leaves the client's own
/// deadline in charge.
///
/// Applied per request rather than on [`HTTP_CLIENT`] because that static is
/// built lazily at first use, which may be before or after the config is read.
/// A per-request timeout also wins over the client's, so this is the value that
/// actually governs.
static HTTP_TIMEOUT_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);

/// Apply `[limits] script_http_timeout_secs` for this process.
pub fn set_script_http_timeout(secs: u64) {
    HTTP_TIMEOUT_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
}

/// The configured per-request deadline, if one is set.
fn script_http_timeout() -> Option<Duration> {
    match HTTP_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// How many extra attempts a script tool's request gets after a transport
/// failure that looks transient.
const SCRIPT_HTTP_RETRIES: u32 = 2;

/// Whether *redirect hops* may land on loopback / private / link-local
/// addresses.
///
/// The authoritative check is [`DaemonScriptHost::allow_local_network`], a plain
/// field on the host. This atomic exists only because [`HTTP_CLIENT`] is
/// process-wide and its redirect callback runs inside reqwest with no access to
/// the host that started the request. `[security] allow_local_network` is a
/// machine-wide switch, so one value per process is the right granularity -
/// but keep the field authoritative and this a mirror of it, not the reverse:
/// global mutable state read by the main check would make every test that
/// touches it race with every test that doesn't.
///
/// Defaults to `false`, so a path that forgets to initialize it is the safe one.
static ALLOW_LOCAL_REDIRECTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The lock every test that touches [`ALLOW_LOCAL_REDIRECTS`] must hold.
///
/// The atomic is process-wide, so in a test binary - where everything runs in
/// one process, in parallel - a test that writes it races every test that reads
/// it, and the loser sees the other test's value with no hint that is what
/// happened. A refusal test quietly succeeds, or a permitted-hop test is refused
/// and blames the thing it was actually checking.
///
/// It lives next to the atomic rather than inside one module's test block
/// because the writers are not all in one module: `setup_daemon_host_with`
/// mirrors the config into it at daemon start-up, so a test that stands up a
/// host is a writer too - and that was the one with no idea it had to take this.
/// An async mutex rather than a `std` one because the writers await: standing up
/// a daemon host is an `async fn`, so a `std` guard held across it is
/// `clippy::await_holding_lock` - and the lint is right, the guard would be
/// pinned to whatever thread the future resumed on. This one also cannot be
/// poisoned, which matters for a lock every test in three modules takes: a test
/// that panicked holding it has already said why, and failing every later test
/// as well would bury that one real failure.
#[cfg(test)]
pub(crate) static REDIRECT_MIRROR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take the [`REDIRECT_MIRROR`] lock from synchronous code.
///
/// Async callers should `REDIRECT_MIRROR.lock().await` instead; this is for the
/// plain `#[test]`s and for a `spawn_blocking` body, neither of which can.
#[cfg(test)]
pub(crate) fn lock_redirect_mirror() -> tokio::sync::MutexGuard<'static, ()> {
    REDIRECT_MIRROR.blocking_lock()
}

/// Apply `[security] allow_local_network` to redirect following for this process.
pub fn set_local_network_allowed(allow: bool) {
    ALLOW_LOCAL_REDIRECTS.store(allow, std::sync::atomic::Ordering::Relaxed);
}

/// The current value of the [`ALLOW_LOCAL_REDIRECTS`] switch.
fn local_network_allowed() -> bool {
    ALLOW_LOCAL_REDIRECTS.load(std::sync::atomic::Ordering::Relaxed)
}

impl RealScriptIo {
    /// A handle on the shared [`HTTP_CLIENT`] (cloning a `Client` shares its
    /// connection pool; it does not build a new one).
    fn client() -> reqwest::blocking::Client {
        HTTP_CLIENT.clone()
    }

    /// Apply a header map to a blocking request builder.
    fn with_headers(
        mut req: reqwest::blocking::RequestBuilder,
        headers: BTreeMap<String, String>,
    ) -> reqwest::blocking::RequestBuilder {
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req
    }

    /// Send a built request and read its body as text.
    ///
    /// A body the `Content-Type` marks as binary is refused rather than decoded.
    /// `Response::text` decodes *anything* lossily, so a PNG or MP3 came back as
    /// a page of U+FFFD replacement characters reported as a **successful**
    /// fetch - no error, no signal, straight into the model's context.
    fn send(
        url: &str,
        build: &dyn Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<String, String> {
        Self::send_capped(url, build, MAX_RESPONSE_BYTES)
    }

    /// Send `req`, retrying a transport failure that looks transient, and
    /// holding a per-host permit for the duration so a batched fan-out cannot
    /// open an unbounded number of connections to one origin.
    ///
    /// Takes a builder *factory* rather than a `RequestBuilder`, because `send`
    /// consumes the builder and `try_clone` has a `None` arm (a streaming body)
    /// that no script tool can reach - a branch nothing could ever test. Asking
    /// the caller to rebuild removes it.
    fn send_with_retry(
        url: &str,
        build: &dyn Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response, String> {
        let _permit = HostPermit::acquire(url);
        let mut attempt = 0;
        loop {
            let req = match script_http_timeout() {
                Some(t) => build().timeout(t),
                None => build(),
            };
            match req.send() {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let chain = error_chain(&e);
                    if attempt >= SCRIPT_HTTP_RETRIES
                        || !leviath_providers::rhai_provider::host::is_retryable_transport(&chain)
                    {
                        return Err(format!("request failed: {chain}"));
                    }
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(200 * u64::from(attempt)));
                }
            }
        }
    }

    /// [`send`](Self::send) with the body cap injected, so the oversized-body
    /// refusal is testable against a small response instead of a 32 MiB one.
    fn send_capped(
        url: &str,
        build: &dyn Fn() -> reqwest::blocking::RequestBuilder,
        max: u64,
    ) -> Result<String, String> {
        // One `send()` used to be the whole story here, and a single HTTP/2
        // stream error therefore lost the page for good: a research run cited
        // two primary sources it had never opened because of exactly this.
        let resp = Self::send_with_retry(url, build)?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if is_binary_content_type(&content_type) {
            let len = resp.content_length();
            return Err(non_text_body_message(&content_type, len));
        }
        // Refuse an oversized body before reading a byte of it. `text()` buffers
        // the whole response, so a server advertising a multi-gigabyte
        // `text/plain` is a memory-exhaustion DoS the 900 KB output cap below
        // does nothing about - that cap runs *after* the allocation.
        //
        // Residual: a chunked response sends no `Content-Length`, so a body that
        // lies about its size is still buffered. The client's 30-second timeout
        // is what bounds that case; closing it properly needs a streaming decoder
        // that preserves `text()`'s charset handling (it decodes Shift-JIS and
        // Latin-1 pages correctly, which a raw `Read` + `from_utf8` would not).
        if let Some(msg) = oversized_body_message(resp.content_length(), max) {
            return Err(msg);
        }
        let text = resp.text().map_err(|e| format!("read body: {e}"))?;
        if let Some(msg) = mojibake_message(&text) {
            return Err(msg);
        }
        let text = cap_script_io(text);
        if status.is_success() {
            Ok(text)
        } else {
            Err(format!("http {status}: {text}"))
        }
    }
}

/// Media types that are never text, so decoding them would only produce noise.
///
/// The check is on the declared type, deliberately **not** on UTF-8 validity of
/// the bytes: `Response::text` is charset-aware and decodes Shift-JIS,
/// ISO-8859-1 and Windows-1252 pages *correctly*, and a strict `from_utf8` test
/// would misclassify exactly those as binary - the non-English pages a
/// researcher agent is most likely to fetch. Anything unrecognised (including a
/// missing header) falls through to the existing text path.
const BINARY_CONTENT_PREFIXES: &[&str] = &[
    "image/",
    "audio/",
    "video/",
    "font/",
    "application/octet-stream",
    "application/pdf",
    "application/zip",
    "application/gzip",
    "application/x-tar",
    "application/x-bzip",
    "application/wasm",
    "application/vnd.",
    "application/msword",
];

/// Whether a `Content-Type` header names content this tool cannot render as text.
fn is_binary_content_type(content_type: &str) -> bool {
    // Trim parameters (`image/png; charset=binary`) and normalise case.
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    // `application/xml`, `+json`, `+xml` etc. are structured *text* despite the
    // `application/` prefix, so match on the concrete list rather than the tree.
    BINARY_CONTENT_PREFIXES
        .iter()
        .any(|prefix| essence.starts_with(prefix))
}

/// Share of replacement characters above which a decoded body is mojibake
/// rather than text. Real prose in any charset stays far under this; a body
/// that was never text at all lands near 1.0, because every byte the decoder
/// could not interpret becomes U+FFFD.
const MOJIBAKE_REPLACEMENT_SHARE: f64 = 0.1;

/// The refusal for a body that decoded into replacement characters, or `None`
/// when the text is usable.
///
/// This is the case [`is_binary_content_type`] cannot catch: a response that
/// declares `text/html` and answers with compressed or otherwise binary bytes.
/// The decode does not fail, it succeeds lossily, so before this the agent
/// received a page of U+FFFD and cited it as though it had read the article.
/// The test is on the *decoded* text, never on raw UTF-8 validity, so a
/// correctly decoded Shift-JIS or Windows-1252 page still passes.
fn mojibake_message(text: &str) -> Option<String> {
    let total = text.chars().count();
    if total == 0 {
        return None;
    }
    let replacements = text
        .chars()
        .filter(|c| *c == char::REPLACEMENT_CHARACTER)
        .count();
    if (replacements as f64) < (total as f64) * MOJIBAKE_REPLACEMENT_SHARE {
        return None;
    }
    Some(format!(
        "body did not decode as text ({replacements} of {total} characters were unreadable) \
         - the response was probably compressed or binary despite its content type"
    ))
}

/// The diagnostic a script tool sees for a binary body. Phrased for the model:
/// it names the type and size so the agent can pick a different source.
fn non_text_body_message(content_type: &str, len: Option<u64>) -> String {
    let size = match len {
        Some(bytes) => format!(", {} KB", bytes.div_ceil(1024)),
        None => String::new(),
    };
    format!("non-text content ({content_type}{size}) - this tool returns text only")
}

/// Cap a host-I/O string below the tool engine's 1 MB `max_string_size`
/// (`build_tool_engine`) so an oversized fetch/read/shell result can't raise the
/// NON-CATCHABLE `ErrorDataTooLarge` inside a Rhai tool script (it aborts the tool
/// even inside try/catch). This is only a crash guard - context-size truncation is
/// handled downstream by region budgets and any in-script truncation.
const MAX_SCRIPT_IO_BYTES: usize = 900_000;

/// Largest response body [`RealScriptIo::send`] will read, checked against the
/// declared `Content-Length` *before* buffering.
///
/// Well above [`MAX_SCRIPT_IO_BYTES`] on purpose: a page a little larger than the
/// output cap should still be fetched and truncated (that is the normal case for
/// a long article), while a body two orders of magnitude larger is refused
/// outright as a resource-exhaustion attempt rather than allocated first.
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// The refusal message for an over-large declared body, or `None` to proceed.
///
/// Split out as a pure function with an injectable `max` so the threshold is
/// testable without a 32 MB HTTP round trip - and because a mock server cannot
/// help here anyway: hyper panics rather than send a `Content-Length` that
/// disagrees with the body it is writing, so the lying-header case that motivates
/// the check is unreachable from an honest test server.
fn oversized_body_message(content_length: Option<u64>, max: u64) -> Option<String> {
    match content_length {
        Some(len) if len > max => Some(format!(
            "response declares {len} bytes, over the {max}-byte limit - \
             fetch a more specific page"
        )),
        _ => None,
    }
}

pub(crate) fn cap_script_io(mut s: String) -> String {
    if s.len() > MAX_SCRIPT_IO_BYTES {
        // Cut on a char boundary - a raw byte cut-off lands mid-character on
        // multi-byte text and panics (the shape of issue #109).
        s.truncate(floor_char_boundary(&s, MAX_SCRIPT_IO_BYTES));
        s.push_str("\n[...truncated by leviath: response exceeded 900 KB]");
    }
    s
}

impl ScriptIo for RealScriptIo {
    fn http_get(&self, url: &str, headers: BTreeMap<String, String>) -> Result<String, String> {
        let client = Self::client();
        Self::send(url, &|| {
            Self::with_headers(client.get(url), headers.clone())
        })
    }

    fn http_post(
        &self,
        url: &str,
        body: &str,
        headers: BTreeMap<String, String>,
    ) -> Result<String, String> {
        let client = Self::client();
        Self::send(url, &|| {
            Self::with_headers(client.post(url).body(body.to_string()), headers.clone())
        })
    }

    fn run_shell(&self, mut cmd: TokioCommand, timeout: Duration) -> Result<String, String> {
        // The script engine drives this from a `spawn_blocking` thread (not a
        // runtime worker), so blocking on the current runtime is safe here and
        // lets us reuse tokio's timeout - the same mechanism the built-in shell
        // tool uses. `try_current` rather than `current`: a blocking thread can
        // outlive runtime shutdown, and `current` would *panic* there - and a
        // panic inside a Rhai native call is the shape that can abort the
        // daemon (issue #109).
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Err("shell is unavailable: no tokio runtime on this thread".to_string());
        };
        // Reap the whole command tree if the future is dropped (timeout, or the
        // batch dropped because the agent was cancelled) rather than detaching
        // it. `kill_on_drop` covers the shell; its own children are reparented
        // to init unless the group is signalled - see `leviath_tools`' shell
        // tool, which does the same.
        cmd.kill_on_drop(true);
        leviath_tools::own_process_group(&mut cmd);
        // `spawn` inherits stdio where `output` pipes it; pipe explicitly so the
        // command's output is still captured.
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        handle.block_on(async move {
            // Spawn inside the timed future so the reaper guard lives exactly as
            // long as the command: dropping the future drops the guard, which
            // signals the group. One fallible block also keeps a single error
            // arm, as `Command::output()` had.
            let run = async {
                let child = cmd.spawn()?;
                let _reaper = child.id().map(leviath_tools::ProcessGroupReaper);
                child.wait_with_output().await
            };
            match tokio::time::timeout(timeout, run).await {
                Ok(Ok(output)) => Ok(cap_script_io(combine_shell_output(
                    &output.stdout,
                    &output.stderr,
                ))),
                Ok(Err(e)) => Err(format!("failed to spawn shell: {e}")),
                Err(_) => Err(format!(
                    "shell command timed out after {}s",
                    timeout.as_secs()
                )),
            }
        })
    }

    fn read_file(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path)
            .map(cap_script_io)
            .map_err(|e| format!("read '{}': {e}", path.display()))
    }

    fn write_file(&self, path: &Path, content: &str) -> Result<String, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir '{}': {e}", parent.display()))?;
        }
        std::fs::write(path, content).map_err(|e| format!("write '{}': {e}", path.display()))?;
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }

    fn env_var(&self, name: &str) -> Result<String, String> {
        std::env::var(name).map_err(|_| format!("environment variable '{name}' is not set"))
    }
}

/// The system shell + command flag for the current platform.
///
/// Deliberately `/bin/sh` on Unix rather than the user's `$SHELL`, unlike the
/// `shell` tool's `BuiltinTools::detect_shell`: a Rhai tool script is authored
/// once and run on every machine, so it gets the POSIX shell it can count on
/// instead of whatever interactive shell the operator happens to prefer.
pub(crate) fn default_shell() -> (&'static str, &'static str) {
    default_shell_for(std::env::consts::OS)
}

/// [`default_shell`] with the platform as a parameter.
///
/// Pure over the OS string rather than `#[cfg(windows)]`-switched, following
/// `leviath_sys::browser::open_command_for`, so the Windows answer is reachable
/// under test on every platform instead of only on the Windows CI leg.
pub(crate) fn default_shell_for(os: &str) -> (&'static str, &'static str) {
    match os {
        "windows" => ("cmd.exe", "/C"),
        _ => ("/bin/sh", "-c"),
    }
}

/// Build the host (un-sandboxed) shell command pointed at `workdir` - the
/// no-sandbox arm of [`DaemonScriptHost::shell`].
pub(crate) fn host_shell_command(
    shell: &str,
    flag: &str,
    command: &str,
    workdir: &Path,
) -> TokioCommand {
    let mut c = leviath_sys::child_command_async(shell);
    c.arg(flag).arg(command).current_dir(workdir);
    c
}

/// Combine a finished command's stdout and (non-empty) stderr into one string,
/// preserving the prior `shell()` contract.
pub(crate) fn combine_shell_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::from_utf8_lossy(stdout).into_owned();
    let err = String::from_utf8_lossy(stderr);
    if !err.trim().is_empty() {
        out.push_str(&err);
    }
    out
}

#[cfg(test)]
mod tests {

    /// A server that drops the first connection without answering, then serves
    /// normally. Reproduces the "the socket did not work this time" shape that
    /// used to lose a source outright.
    ///
    /// A blocking listener over exactly two connections, so the spawned body
    /// *returns*. An `accept` loop that runs forever leaves its own closing
    /// region uncovered, which is why [`mock_http`] hands `tokio::spawn` a
    /// future with no block of ours in it.
    fn flaky_http() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (i, mut sock) in listener.incoming().take(2).flatten().enumerate() {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf);
                // The first caller gets a FIN mid-request and nothing else.
                if i > 0 {
                    let body = "RETRIED-OK";
                    let _ = sock.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                }
            }
        });
        format!("http://{addr}")
    }

    /// Before the retry, one dropped connection was the whole story: the page
    /// was lost and the bibliography still cited it.
    #[test]
    fn a_dropped_connection_is_retried_rather_than_lost() {
        let base = flaky_http();
        let client = RealScriptIo::client();
        let url = format!("{base}/x");
        let out = RealScriptIo::send(&url, &|| client.get(&url));
        assert_eq!(out.expect("the retry recovers"), "RETRIED-OK");
    }

    /// A host that never answers still gives up rather than looping, and says
    /// so in the message the agent reads.
    #[test]
    fn a_dead_host_gives_up_with_a_named_failure() {
        let _guard = lock_http_limits();
        let previous = HTTP_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed);
        // Also the no-deadline arm: `0` leaves the client's own timeout in
        // charge rather than stamping one per request.
        set_script_http_timeout(0);
        let client = RealScriptIo::client();
        // Nothing listening: connection refused is not retryable, so this
        // returns on the first attempt instead of spending the budget.
        let out = RealScriptIo::send("http://127.0.0.1:19997/x", &|| {
            client.get("http://127.0.0.1:19997/x")
        });
        set_script_http_timeout(previous);
        let err = out.expect_err("a dead host fails");
        assert!(err.contains("request failed"), "got: {err}");
    }

    // ─── per-host request gate ──────────────────────────────────────────────

    /// `HTTP_MAX_PER_HOST` and `HTTP_TIMEOUT_SECS` are process-wide, so every
    /// test that writes one races every test that reads it - the same hazard
    /// [`REDIRECT_MIRROR`] exists for, and it bit exactly the same way: a
    /// concurrent test raising the cap made the "over the cap" test's second
    /// request sail straight through.
    static HTTP_LIMITS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the script-HTTP limits lock.
    fn lock_http_limits() -> std::sync::MutexGuard<'static, ()> {
        HTTP_LIMITS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn host_of_lowercases_and_ignores_unparseable_urls() {
        assert_eq!(
            host_of("https://Example.COM/a/b"),
            Some("example.com".into())
        );
        assert_eq!(host_of("http://127.0.0.1:8080/x"), Some("127.0.0.1".into()));
        assert_eq!(host_of("not a url"), None);
        // A parseable URL with no host still yields nothing to key a permit on.
        assert_eq!(host_of("data:text/plain,hi"), None);
    }

    #[test]
    fn a_permit_is_held_then_released() {
        let _guard = lock_http_limits();
        let previous = HTTP_MAX_PER_HOST.load(std::sync::atomic::Ordering::Relaxed);
        set_script_http_max_per_host(2);
        {
            let _a = HostPermit::acquire("https://gate.example/a");
            let _b = HostPermit::acquire("https://gate.example/b");
            let held = IN_FLIGHT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get("gate.example")
                .copied();
            assert_eq!(held, Some(2), "both permits counted against the host");
            // A different host is counted separately, so one slow origin cannot
            // stall fetches to every other one.
            let _c = HostPermit::acquire("https://other.example/c");
            assert_eq!(
                IN_FLIGHT
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get("other.example")
                    .copied(),
                Some(1)
            );
        }
        // Dropping every permit removes the key rather than leaving a zero.
        assert!(
            !IN_FLIGHT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key("gate.example")
        );
        set_script_http_max_per_host(previous);
    }

    #[test]
    fn an_unbounded_cap_takes_no_permit_at_all() {
        let _guard = lock_http_limits();
        let previous = HTTP_MAX_PER_HOST.load(std::sync::atomic::Ordering::Relaxed);
        set_script_http_max_per_host(0);
        let _p = HostPermit::acquire("https://unbounded.example/x");
        assert!(
            !IN_FLIGHT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key("unbounded.example"),
            "0 means unlimited, so nothing is tracked"
        );
        set_script_http_max_per_host(previous);
    }

    #[test]
    fn a_request_over_the_cap_waits_for_a_slot() {
        let _guard = lock_http_limits();
        let previous = HTTP_MAX_PER_HOST.load(std::sync::atomic::Ordering::Relaxed);
        set_script_http_max_per_host(1);
        let held = HostPermit::acquire("https://queue.example/first");
        let waiter = std::thread::spawn(|| {
            let _second = HostPermit::acquire("https://queue.example/second");
            "got in"
        });
        // The second request cannot proceed while the first holds the only slot.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !waiter.is_finished(),
            "the cap did not hold the second request"
        );
        drop(held);
        assert_eq!(waiter.join().expect("waiter finishes"), "got in");
        set_script_http_max_per_host(previous);
    }

    #[test]
    fn the_http_timeout_switch_round_trips() {
        let _guard = lock_http_limits();
        let previous = HTTP_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed);
        set_script_http_timeout(7);
        assert_eq!(script_http_timeout(), Some(Duration::from_secs(7)));
        // Zero hands the deadline back to the client rather than meaning "now".
        set_script_http_timeout(0);
        assert_eq!(script_http_timeout(), None);
        set_script_http_timeout(previous);
    }
    use super::*;
    use std::sync::Mutex;

    // ── resolve_script_permissions ──

    fn perms(all: ScriptPermission) -> ScriptToolPermissions {
        ScriptToolPermissions {
            http_get: all,
            http_post: all,
            shell: all,
            read_file: all,
            write_file: all,
            env_var: all,
        }
    }

    #[test]
    fn resolve_allow_permits_everything() {
        let a = resolve_script_permissions(&perms(ScriptPermission::Allow), &|_| ToolPolicy::Deny);
        assert_eq!(
            a,
            ScriptAllow {
                http_get: true,
                http_post: true,
                shell: true,
                read_file: true,
                write_file: true,
                env_var: true,
            }
        );
    }

    #[test]
    fn resolve_deny_blocks_everything() {
        let a = resolve_script_permissions(&perms(ScriptPermission::Deny), &|_| ToolPolicy::Allow);
        assert_eq!(
            a,
            ScriptAllow {
                http_get: false,
                http_post: false,
                shell: false,
                read_file: false,
                write_file: false,
                env_var: false,
            }
        );
    }

    #[test]
    fn resolve_inherit_net_true_filelike_follows_builtin() {
        // Default is Inherit. Builtin resolves read_file→Allow, shell→Ask.
        let a = resolve_script_permissions(&ScriptToolPermissions::default(), &|name| match name {
            "read_file" => ToolPolicy::Allow,
            _ => ToolPolicy::Ask,
        });
        assert!(a.http_get && a.http_post && a.env_var);
        assert!(a.read_file, "read_file inherit → Allow");
        assert!(!a.write_file, "write_file inherit → Ask ⇒ denied");
        assert!(!a.shell, "shell inherit → Ask ⇒ denied");
    }

    // ── effective_script_permissions (per-agent override) ──

    #[test]
    fn effective_perms_agent_tightens_per_field() {
        // Global allows everything; the agent's blueprint tightens several
        // fields (exercising the allow/deny/inherit parse arms) and leaves the
        // rest at the global value.
        let global = perms(ScriptPermission::Allow);
        let manifest = "\
            [tool_script_permissions]\n\
            http_get = \"allow\"\n\
            shell = \"deny\"\n\
            write_file = \"inherit\"\n";
        let eff = effective_script_permissions(&global, manifest);
        assert_eq!(eff.http_get, ScriptPermission::Allow, "allow arm");
        assert_eq!(eff.shell, ScriptPermission::Deny, "deny arm");
        assert_eq!(eff.write_file, ScriptPermission::Inherit, "inherit arm");
        assert_eq!(eff.env_var, ScriptPermission::Allow, "unset keeps global");
        assert_eq!(eff.read_file, ScriptPermission::Allow);
        assert_eq!(eff.http_post, ScriptPermission::Allow);
    }

    /// The manifest may not loosen what the user locked down. The other way
    /// round - a downloaded agent setting `http_get = "allow"` over a global
    /// `deny` getting the network back - makes the user's config advisory
    /// rather than binding.
    #[test]
    fn effective_perms_agent_cannot_loosen_global() {
        let global = perms(ScriptPermission::Deny);
        let manifest = "\
            [tool_script_permissions]\n\
            http_get = \"allow\"\n\
            shell = \"allow\"\n\
            env_var = \"inherit\"\n";
        let eff = effective_script_permissions(&global, manifest);
        assert_eq!(eff.http_get, ScriptPermission::Deny);
        assert_eq!(eff.shell, ScriptPermission::Deny);
        assert_eq!(eff.env_var, ScriptPermission::Deny);
    }

    /// `Inherit` sits between `Allow` and `Deny`, so a manifest cannot promote an
    /// inherited file/shell permission to an unconditional allow either.
    #[test]
    fn effective_perms_agent_cannot_promote_inherit_to_allow() {
        let global = perms(ScriptPermission::Inherit);
        let manifest = "[tool_script_permissions]\nshell = \"allow\"\n";
        let eff = effective_script_permissions(&global, manifest);
        assert_eq!(eff.shell, ScriptPermission::Inherit);
    }

    #[test]
    fn effective_perms_absent_section_keeps_global() {
        let global = perms(ScriptPermission::Deny);
        // No section at all → global unchanged.
        let eff = effective_script_permissions(&global, "[agent]\nname = \"x\"");
        assert_eq!(eff.shell, ScriptPermission::Deny);
        assert_eq!(eff.http_get, ScriptPermission::Deny);
    }

    #[test]
    fn effective_perms_malformed_inputs_fall_back_to_global() {
        let global = perms(ScriptPermission::Allow);
        // Unparseable TOML → global unchanged.
        let eff = effective_script_permissions(&global, "not = valid = toml");
        assert_eq!(eff.shell, ScriptPermission::Allow);
        // Present-but-not-a-table → global unchanged.
        let eff2 = effective_script_permissions(&global, "tool_script_permissions = 5");
        assert_eq!(eff2.shell, ScriptPermission::Allow);
        // An unrecognized value inside the table → that field keeps the global.
        let eff3 =
            effective_script_permissions(&global, "[tool_script_permissions]\nshell = \"maybe\"");
        assert_eq!(eff3.shell, ScriptPermission::Allow);
    }

    // ── permission gates on the host ──

    struct RecordingIo {
        calls: Mutex<Vec<String>>,
    }
    impl RecordingIo {
        fn arc() -> Arc<RecordingIo> {
            Arc::new(RecordingIo {
                calls: Mutex::new(Vec::new()),
            })
        }
    }
    impl ScriptIo for RecordingIo {
        fn http_get(&self, url: &str, _h: BTreeMap<String, String>) -> Result<String, String> {
            self.calls.lock().unwrap().push(format!("get:{url}"));
            Ok("g".into())
        }
        fn http_post(
            &self,
            url: &str,
            body: &str,
            _h: BTreeMap<String, String>,
        ) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("post:{url}:{body}"));
            Ok("p".into())
        }
        fn run_shell(&self, cmd: TokioCommand, _timeout: Duration) -> Result<String, String> {
            // Record the prepared program (host `sh`/`cmd.exe` when un-sandboxed).
            let prog = cmd.as_std().get_program().to_string_lossy().into_owned();
            self.calls.lock().unwrap().push(format!("shell:{prog}"));
            Ok("s".into())
        }
        fn read_file(&self, path: &Path) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("read:{}", path.display()));
            Ok("r".into())
        }
        fn write_file(&self, path: &Path, content: &str) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("write:{}:{content}", path.display()));
            Ok("w".into())
        }
        fn env_var(&self, name: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(format!("env:{name}"));
            Ok("e".into())
        }
    }

    fn all_allowed() -> ScriptAllow {
        ScriptAllow {
            http_get: true,
            http_post: true,
            shell: true,
            read_file: true,
            write_file: true,
            env_var: true,
        }
    }

    fn none_allowed() -> ScriptAllow {
        ScriptAllow {
            http_get: false,
            http_post: false,
            shell: false,
            read_file: false,
            write_file: false,
            env_var: false,
        }
    }

    /// A script tool is the other spelling of "run a shell command", and it
    /// bypassed `clamp_by_effect` entirely - that clamp lives in the tool
    /// dispatcher, which a Rhai `shell()` never goes through. So an agent
    /// shipping its own `.rhai` tools could write through a redirect while
    /// `write_file` was denied, which is exactly what the clamp exists to stop.
    #[test]
    fn a_script_shell_redirect_answers_to_the_write_permission() {
        let io = RecordingIo::arc();
        let allow = ScriptAllow {
            write_file: false,
            ..all_allowed()
        };
        let host = DaemonScriptHost::with_io(allow, std::env::temp_dir(), io.clone());

        let err = host
            .shell("echo pwn > /root/.bashrc")
            .expect_err("a redirect must answer to the write permission");
        assert!(err.contains("write_file"), "got: {err}");

        // The same command without the redirect still runs, so this is the
        // write being refused rather than the shell.
        host.shell("echo pwn").expect("a non-writing shell is fine");

        // And with writes permitted, a redirect *inside the workdir* runs.
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        host.shell("echo pwn > x")
            .expect("a permitted write is not clamped");
    }

    /// Issue #289. `allow.write_file` answers "may this write at all"; it does
    /// not answer "may it write *there*". This host's `write_file` is
    /// workdir-confined, so its `shell()` redirects are too - otherwise a script
    /// with writes permitted could put a file anywhere on the host.
    #[test]
    fn a_script_shell_redirect_stays_inside_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), dir.path().to_path_buf(), io.clone());

        let err = host
            .shell("echo pwn > /root/.bashrc")
            .expect_err("an escaping redirect is refused even with writes allowed");
        assert!(err.contains("outside the working directory"), "got: {err}");

        // The control: inside the workdir it still runs, so this is the path
        // being refused rather than every redirect.
        host.shell("echo ok > inside.txt")
            .expect("a redirect inside the workdir runs");
    }

    #[test]
    fn script_write_refuses_a_deleted_workspace() {
        // Same rule as the built-in write tools (#107): a script may not
        // resurrect a workspace that disappeared out from under the run.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("gone");
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), workdir.clone(), io.clone());
        let err = host.write_file("out.txt", "body").unwrap_err();
        assert!(err.contains("no longer accessible"), "got: {err}");
        assert!(
            io.calls.lock().unwrap().is_empty(),
            "the io layer never ran"
        );
        // A live workspace still writes.
        std::fs::create_dir(&workdir).unwrap();
        assert_eq!(host.write_file("out.txt", "body").unwrap(), "w");
    }

    /// A public IP *literal*, not a hostname: the outbound check resolves names,
    /// and a unit test must not depend on DNS (or on the network being up) to
    /// decide whether the host delegates to its I/O backend.
    const PUBLIC_URL: &str = "http://93.184.216.34/";

    #[test]
    fn allowed_calls_delegate_to_io() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        assert_eq!(host.http_get(PUBLIC_URL, BTreeMap::new()).unwrap(), "g");
        assert_eq!(
            host.http_post(PUBLIC_URL, "b", BTreeMap::new()).unwrap(),
            "p"
        );
        assert_eq!(host.shell("ls").unwrap(), "s");
        assert_eq!(host.write_file("out.txt", "body").unwrap(), "w");
        assert_eq!(host.env_var("HOME").unwrap(), "e");
        let calls = io.calls.lock().unwrap().clone();
        assert!(calls.contains(&format!("get:{PUBLIC_URL}")));
        assert!(calls.iter().any(|c| c.starts_with("post:")));
        // Un-sandboxed → the prepared command runs the host shell.
        assert!(calls.iter().any(|c| c.starts_with("shell:")));
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("write:") && c.ends_with(":body"))
        );
        assert!(calls.contains(&"env:HOME".to_string()));
    }

    /// The exfiltration/SSRF case: a script tool with `http_get` permission is
    /// still not a licence to reach the user's own network. Nothing may touch
    /// the I/O backend - the URL is refused before a request is built.
    #[test]
    fn outbound_check_blocks_local_targets_before_any_io() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        for url in [
            // Cloud metadata: returns instance credentials.
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            // The user's own agent-spawning API.
            "http://127.0.0.1:3000/api/agents",
            // The LAN.
            "http://192.168.1.1/",
            // Not an HTTP scheme at all.
            "file:///etc/passwd",
        ] {
            let err = host.http_get(url, BTreeMap::new()).unwrap_err();
            assert!(err.starts_with("[denied]"), "{url} → {err}");
            let err = host.http_post(url, "leak", BTreeMap::new()).unwrap_err();
            assert!(err.starts_with("[denied]"), "{url} → {err}");
        }
        let calls = io.calls.lock().unwrap().clone();
        assert!(
            calls.is_empty(),
            "a refused URL must never reach the I/O backend: {calls:?}"
        );
    }

    /// The exfiltration half of the chain: a `.rhai` tool that ships inside an
    /// installed agent bundle calling `env_var("ANTHROPIC_API_KEY")`. Paired with
    /// the SSRF guard above, the two-line "read a key, POST it out" script no
    /// longer has either half available to it.
    #[test]
    fn env_var_refuses_credential_names_by_default() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "LEVIATH_API_TOKEN",
        ] {
            let err = host.env_var(name).unwrap_err();
            assert!(err.starts_with("[denied]"), "{name} → {err}");
            assert!(err.contains("allow_env_vars"), "{name} → {err}");
        }
        assert!(
            io.calls.lock().unwrap().is_empty(),
            "a refused read must never reach the I/O backend"
        );
    }

    /// Ordinary variables are unaffected - a script reading `PATH` or its own
    /// app's setting is normal, and the gate would be useless if it broke that.
    #[test]
    fn env_var_allows_ordinary_names() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        assert_eq!(host.env_var("PATH").unwrap(), "e");
        assert_eq!(host.env_var("MY_APP_REGION").unwrap(), "e");
    }

    /// The user allowlisting a name is them saying "yes, this agent is meant to
    /// have that one" - and only that one.
    #[test]
    fn env_var_allowlist_permits_exactly_the_named_variable() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone())
            .with_env_allowlist(vec!["MY_PROVIDER_KEY".to_string()]);
        assert_eq!(host.env_var("MY_PROVIDER_KEY").unwrap(), "e");
        assert!(host.env_var("ANTHROPIC_API_KEY").is_err());
    }

    /// A malformed URL is refused rather than passed through for the HTTP client
    /// to interpret.
    #[test]
    fn outbound_check_rejects_unparseable_urls() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        let err = host.http_get("not a url", BTreeMap::new()).unwrap_err();
        assert!(err.contains("invalid URL"), "{err}");
        assert!(io.calls.lock().unwrap().is_empty());
    }

    /// `[security] allow_local_network = true` is what a user running a local
    /// model (Ollama on 11434, say) sets. It is a field on the host, not global
    /// state, so this test cannot perturb any other.
    #[test]
    fn allow_local_network_opens_the_local_path() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone())
            .with_local_network(true);
        assert_eq!(
            host.http_get("http://127.0.0.1:11434/api/tags", BTreeMap::new())
                .unwrap(),
            "g"
        );
        // The scheme check is not waived by it.
        assert!(
            host.http_get("file:///etc/passwd", BTreeMap::new())
                .is_err()
        );
    }

    /// The redirect mirror is a separate process-wide value; setting it must not
    /// change what the host itself decides.
    #[test]
    fn redirect_switch_is_independent_of_the_host_field() {
        let _guard = lock_redirect_mirror();
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        let previous = local_network_allowed();
        set_local_network_allowed(true);
        let decided = host.http_get("http://127.0.0.1:9/", BTreeMap::new());
        set_local_network_allowed(previous);
        assert!(
            decided.is_err(),
            "the host field, not the redirect mirror, decides the initial URL"
        );
    }

    #[test]
    fn denied_calls_return_denied_and_skip_io() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(none_allowed(), std::env::temp_dir(), io.clone());
        assert!(
            host.http_get("http://x", BTreeMap::new())
                .unwrap_err()
                .contains("[denied]")
        );
        assert!(
            host.http_post("http://x", "b", BTreeMap::new())
                .unwrap_err()
                .contains("http_post")
        );
        assert!(host.shell("ls").unwrap_err().contains("shell"));
        assert!(host.read_file("a.txt").unwrap_err().contains("read_file"));
        assert!(
            host.write_file("a.txt", "b")
                .unwrap_err()
                .contains("write_file")
        );
        assert!(host.env_var("X").unwrap_err().contains("env_var"));
        assert!(
            io.calls.lock().unwrap().is_empty(),
            "no I/O on denied calls"
        );
    }

    #[test]
    fn read_file_confined_to_workdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "hi").unwrap();
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), dir.path().to_path_buf(), io.clone());
        // Allowed relative path → delegates.
        assert_eq!(host.read_file("ok.txt").unwrap(), "r");
        assert_eq!(host.write_file("ok.txt", "x").unwrap(), "w");
        // Escaping path → rejected before any I/O (both read and write share the
        // resolve_in_workdir `?` guard).
        let err = host.read_file("../../etc/passwd").unwrap_err();
        assert!(err.contains("escape"));
        let werr = host.write_file("../../etc/passwd", "x").unwrap_err();
        assert!(werr.contains("escape"));
        // Only the ok.txt read + write reached the io (the escaping calls did not).
        let calls = io.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|c| c.starts_with("read:")));
        assert!(calls.iter().any(|c| c.starts_with("write:")));
    }

    #[test]
    fn read_file_absolute_outside_workdir_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let host =
            DaemonScriptHost::with_io(all_allowed(), dir.path().to_path_buf(), RecordingIo::arc());
        // A path that is *absolute on the current platform* (a leading `/` is not
        // absolute on Windows - it needs a drive/UNC prefix), and outside the
        // workdir. `temp_dir()` is absolute everywhere and a sibling of the
        // workdir tempdir, so it exercises the `is_absolute()` → true branch.
        let outside = std::env::temp_dir().join("leviath-abs-outside-xyz");
        assert!(outside.is_absolute(), "test path must be absolute");
        let err = host.read_file(outside.to_str().unwrap()).unwrap_err();
        assert!(err.contains("would escape"), "got: {err}");
    }

    #[test]
    fn read_file_pop_past_root_rejected() {
        // A *relative* workdir keeps the component accumulator free of any root
        // prefix, so a second `..` pops an empty accumulator → the "escapes"
        // (pop-fail) branch, distinct from the "would escape" (starts_with) one.
        let host =
            DaemonScriptHost::with_io(all_allowed(), PathBuf::from("wd"), RecordingIo::arc());
        let err = host.read_file("../..").unwrap_err();
        assert!(err.contains("escapes the working directory"), "got: {err}");
    }

    // ── RealScriptIo (hermetic, local) ──

    async fn mock_http() -> String {
        use axum::Router;
        use axum::routing::{get, post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/ok", get(|| async { "GET-BODY" }))
            .route("/echo", post(|body: String| async move { body }))
            .route(
                "/boom",
                get(|| async {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "server error",
                    )
                }),
            )
            // A binary body: `Response::text` would lossily decode this into
            // replacement characters and report success.
            .route(
                "/png",
                get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "image/png")],
                        // A real PNG signature + IHDR-ish bytes; invalid UTF-8.
                        vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe],
                    )
                }),
            )
            // Declared text in a non-UTF-8 charset - must still decode, which is
            // why the guard reads the header rather than testing UTF-8 validity.
            .route(
                "/shiftjis",
                get(|| async {
                    (
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "text/html; charset=shift_jis",
                        )],
                        // "日本語" in Shift-JIS.
                        vec![0x93u8, 0xfa, 0x96, 0x7b, 0x8c, 0xea],
                    )
                }),
            )
            // Declared text, answered with bytes that are not text in any
            // charset: the shape a compressed body arrives in. `text()` decodes
            // it lossily and succeeds, so only the decoded result gives it away.
            .route(
                "/gibberish",
                get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/html")],
                        vec![0x1fu8, 0x8b, 0x08, 0x00, 0xff, 0xfe, 0xfd, 0xfc, 0xfb, 0xfa],
                    )
                }),
            );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    #[test]
    fn binary_content_types_are_classified_but_structured_text_is_not() {
        for text in [
            "",
            "text/html; charset=utf-8",
            "text/plain",
            "application/json",
            "application/xml",
            "application/xhtml+xml",
            "application/ld+json",
            "application/javascript",
        ] {
            assert!(!is_binary_content_type(text), "should be text: {text:?}");
        }
        for binary in [
            "image/png",
            "IMAGE/PNG",
            "image/jpeg; charset=binary",
            "  audio/mpeg  ",
            "video/mp4",
            "font/woff2",
            "application/octet-stream",
            "application/pdf",
            "application/zip",
            "application/gzip",
            "application/x-tar",
            "application/x-bzip2",
            "application/wasm",
            "application/vnd.ms-excel",
            "application/msword",
        ] {
            assert!(
                is_binary_content_type(binary),
                "should be binary: {binary:?}"
            );
        }
    }

    #[test]
    fn the_non_text_diagnostic_names_the_type_and_size_when_known() {
        let with_len = non_text_body_message("image/png", Some(2049));
        assert!(with_len.contains("image/png"), "got: {with_len}");
        assert!(with_len.contains("3 KB"), "rounds up: {with_len}");
        let without_len = non_text_body_message("audio/mpeg", None);
        assert!(without_len.contains("audio/mpeg"), "got: {without_len}");
        assert!(
            !without_len.contains("KB"),
            "no size to report: {without_len}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binary_bodies_are_refused_and_non_utf8_text_still_decodes() {
        let base = mock_http().await;
        let (png, sjis) = tokio::task::spawn_blocking(move || {
            (
                RealScriptIo.http_get(&format!("{base}/png"), BTreeMap::new()),
                RealScriptIo.http_get(&format!("{base}/shiftjis"), BTreeMap::new()),
            )
        })
        .await
        .unwrap();

        // A PNG is refused outright rather than returned as replacement chars.
        let err = png.unwrap_err();
        assert!(err.contains("non-text content"), "got: {err}");
        assert!(err.contains("image/png"), "got: {err}");

        // A Shift-JIS page is text: it must still come back decoded. Guarding on
        // UTF-8 validity instead of the header would have broken this.
        assert_eq!(sjis.unwrap(), "日本語");
    }

    /// A body declaring itself larger than the cap is refused from the header,
    /// before `text()` allocates it. The 900 KB output cap runs *after* the read,
    /// so it was never a defence against this.
    #[test]
    fn oversized_declared_body_is_refused() {
        let msg = oversized_body_message(Some(999_999_999), 1_000).expect("should refuse");
        assert!(msg.contains("999999999"), "{msg}");
        assert!(msg.contains("1000-byte limit"), "{msg}");
    }

    /// The mojibake guard in the real `send` path: a `text/html` response whose
    /// body is not text in any charset is refused rather than handed on.
    #[tokio::test(flavor = "multi_thread")]
    async fn send_refuses_a_body_that_did_not_decode_as_text() {
        let base = mock_http().await;
        let out = tokio::task::spawn_blocking(move || {
            let client = RealScriptIo::client();
            let url = format!("{base}/gibberish");
            RealScriptIo::send(&url, &|| client.get(&url))
        })
        .await
        .expect("join");
        let err = out.expect_err("gibberish is not text");
        assert!(err.contains("did not decode as text"), "{err}");
    }

    /// A body that decoded into replacement characters is refused, and real
    /// text in any charset is not.
    #[test]
    fn mojibake_is_refused_and_ordinary_text_is_not() {
        let msg = mojibake_message("\u{fffd}\u{fffd}\u{fffd}\u{fffd}a").expect("should refuse");
        assert!(msg.contains("4 of 5 characters"), "{msg}");
        assert!(msg.contains("compressed or binary"), "{msg}");
        // Decoded Japanese, a lone stray replacement char in a long page, and
        // an empty body all stay on the text path.
        assert!(mojibake_message("\u{30a6}\u{30a7}\u{30cf}").is_none());
        let mostly_text = format!("{}{}", char::REPLACEMENT_CHARACTER, "a".repeat(20));
        assert!(mojibake_message(&mostly_text).is_none());
        assert!(mojibake_message("").is_none());
    }

    /// A body at or under the cap proceeds, and so does one with no declared
    /// length - a chunked response has none, and refusing every chunked page
    /// would break most of the web.
    #[test]
    fn body_within_cap_or_of_unknown_size_proceeds() {
        assert!(oversized_body_message(Some(1_000), 1_000).is_none());
        assert!(oversized_body_message(Some(0), 1_000).is_none());
        assert!(oversized_body_message(None, 1_000).is_none());
    }

    /// The cap in the real `send` path, against a small response with the limit
    /// lowered - the 32 MiB production value would mean transferring 32 MiB to
    /// assert one branch.
    #[tokio::test(flavor = "multi_thread")]
    async fn send_refuses_a_body_over_the_cap() {
        let base = mock_http().await;
        let out = tokio::task::spawn_blocking(move || {
            let client = RealScriptIo::client();
            // `/ok` returns "GET-BODY" (8 bytes) with a Content-Length.
            let url = format!("{base}/ok");
            RealScriptIo::send_capped(&url, &|| client.get(&url), 4)
        })
        .await
        .unwrap();
        let err = out.expect_err("a body over the cap is refused");
        assert!(err.contains("over the"), "got: {err}");
    }

    /// A redirect is a fresh destination the caller's original URL check never
    /// saw, so the policy re-checks every hop. Here a public-looking request is
    /// bounced to loopback - the shape that turns any redirect-following fetch
    /// into an SSRF primitive.
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_a_local_address_are_refused() {
        use axum::Router;
        use axum::response::Redirect;
        use axum::routing::get;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Only `/bounce` is served: if the guard ever fails open, the request
        // 404s instead of succeeding, and the test still fails - but no handler
        // sits here unreached on the passing path.
        let app = Router::new().route(
            "/bounce",
            get(move || async move { Redirect::temporary(&format!("http://{addr}/ok")) }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));

        // The guard is taken out here and moved into the closure, so it covers
        // the whole request and is released when the closure ends. Taking it
        // inside would mean `blocking_lock` on a runtime worker thread, and
        // awaiting it out here while a `std` guard stayed live would pin the
        // guard to whichever thread the future resumed on.
        let guard = REDIRECT_MIRROR.lock().await;
        let out = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let previous = local_network_allowed();
            set_local_network_allowed(false);
            let result = RealScriptIo.http_get(&format!("http://{addr}/bounce"), BTreeMap::new());
            set_local_network_allowed(previous);
            result
        })
        .await
        .unwrap();
        let err = out.expect_err("a redirect to loopback must not be followed");
        assert!(err.contains("refused to follow redirect"), "got: {err}");
    }

    /// A redirect *loop* is bounded even when every hop is permitted, so a
    /// server cannot hold a fetch open by bouncing it forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_redirect_loop_is_bounded() {
        use axum::Router;
        use axum::response::Redirect;
        use axum::routing::get;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/loop",
            get(move || async move { Redirect::temporary(&format!("http://{addr}/loop")) }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));

        let guard = REDIRECT_MIRROR.lock().await;
        let out = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            // Loopback hops are permitted here, so the *count* is what stops it.
            let previous = local_network_allowed();
            set_local_network_allowed(true);
            let result = RealScriptIo.http_get(&format!("http://{addr}/loop"), BTreeMap::new());
            set_local_network_allowed(previous);
            result
        })
        .await
        .unwrap();
        let err = out.expect_err("an endless redirect must be stopped");
        assert!(err.contains("too many redirects"), "got: {err}");
    }

    /// The script host's own path confinement, mirroring `BuiltinTools`: a
    /// symlink inside the workdir that points outside it is refused.
    #[cfg(unix)]
    #[test]
    fn script_host_read_refuses_a_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("workspace");
        std::fs::create_dir(&workdir).unwrap();
        std::os::unix::fs::symlink("/", workdir.join("link")).unwrap();

        let host = DaemonScriptHost::with_io(all_allowed(), workdir, RecordingIo::arc());
        let err = host.read_file("link/etc/hosts").unwrap_err();
        assert!(err.contains("symlink"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_http_get_success_and_headers() {
        let base = mock_http().await;
        let out = tokio::task::spawn_blocking(move || {
            let mut h = BTreeMap::new();
            h.insert("X-Test".to_string(), "1".to_string());
            RealScriptIo.http_get(&format!("{base}/ok"), h)
        })
        .await
        .unwrap();
        assert_eq!(out.unwrap(), "GET-BODY");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_http_get_non_success_is_error() {
        let base = mock_http().await;
        let out = tokio::task::spawn_blocking(move || {
            RealScriptIo.http_get(&format!("{base}/boom"), BTreeMap::new())
        })
        .await
        .unwrap();
        let err = out.unwrap_err();
        assert!(
            err.contains("http 500") && err.contains("server error"),
            "got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_http_get_connection_error() {
        // Nothing listening on this port → send() fails.
        let out = tokio::task::spawn_blocking(|| {
            RealScriptIo.http_get("http://127.0.0.1:1/x", BTreeMap::new())
        })
        .await
        .unwrap();
        assert!(out.unwrap_err().contains("request failed"));
    }

    /// A raw TCP server that declares a larger Content-Length than it sends, then
    /// closes - so `resp.text()` errors on the incomplete body (mirrors the
    /// package-registry truncated-body test).
    async fn spawn_truncated_body_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = b"partial";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len() + 4096
        )
        .into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_http_body_read_error() {
        let base = spawn_truncated_body_server().await;
        let out = tokio::task::spawn_blocking(move || {
            RealScriptIo.http_get(&format!("{base}/x"), BTreeMap::new())
        })
        .await
        .unwrap();
        let err = out.unwrap_err();
        assert!(err.contains("read body"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_http_post_echoes_body() {
        let base = mock_http().await;
        let out = tokio::task::spawn_blocking(move || {
            RealScriptIo.http_post(&format!("{base}/echo"), "hello", BTreeMap::new())
        })
        .await
        .unwrap();
        assert_eq!(out.unwrap(), "hello");
    }

    /// Build a host command + run it through `run_shell` on a blocking thread
    /// (so its `Handle::block_on` isn't called from a runtime worker).
    async fn run_host_shell(
        command: &'static str,
        workdir: PathBuf,
        timeout: Duration,
    ) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            let (shell, flag) = default_shell();
            let cmd = host_shell_command(shell, flag, command, &workdir);
            RealScriptIo.run_shell(cmd, timeout)
        })
        .await
        .unwrap()
    }

    #[test]
    fn real_shell_off_a_runtime_errors_instead_of_panicking() {
        // A blocking thread can outlive runtime shutdown; `Handle::current()`
        // would panic there, and a panic inside a Rhai native call aborted the
        // whole daemon before issue #109 was fixed. A plain `std::thread` is
        // the same "no reactor on this thread" condition.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let err = std::thread::spawn(move || {
            let (shell, flag) = default_shell();
            let cmd = host_shell_command(shell, flag, "echo hi", &workdir);
            RealScriptIo.run_shell(cmd, Duration::from_secs(5))
        })
        .join()
        .unwrap()
        .unwrap_err();
        assert!(err.contains("no tokio runtime"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_shell_runs_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        // stdout (empty-stderr arm of combine_shell_output)
        let out = run_host_shell(
            "echo hello",
            dir.path().to_path_buf(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(out.contains("hello"));
        // stderr is appended (non-empty stderr arm)
        let out2 = run_host_shell(
            "echo oops 1>&2",
            dir.path().to_path_buf(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(out2.contains("oops"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_shell_spawn_failure() {
        // A non-existent cwd makes the child fail to spawn → the Ok(Err) arm.
        let missing = PathBuf::from("/no/such/workdir/leviath");
        let err = run_host_shell("echo hi", missing, Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.contains("failed to spawn shell"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_shell_times_out() {
        // A slow command against a tiny timeout hits the Err(_) (timeout) arm.
        let dir = tempfile::tempdir().unwrap();
        let err = run_host_shell(
            "sleep 5",
            dir.path().to_path_buf(),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[test]
    fn combine_shell_output_appends_nonempty_stderr_only() {
        // Empty stderr → stdout unchanged; non-empty stderr → appended.
        assert_eq!(combine_shell_output(b"out", b"   "), "out");
        assert_eq!(combine_shell_output(b"out", b"err"), "outerr");
    }

    #[test]
    fn host_shell_command_targets_workdir() {
        let cmd = host_shell_command("sh", "-c", "echo hi", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), "sh");
    }

    #[test]
    fn shell_routes_through_sandbox_when_present() {
        use leviath_core::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};
        // A namespace sandbox with warn-fallback builds a manager on every
        // platform. Attaching it exercises the `Some(sandbox)` arm of `shell()`
        // (the command is built via the manager, not `host_shell_command`).
        let by_index = vec![ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            on_unavailable: OnUnavailable::Warn,
            ..Default::default()
        }];
        let sb = SandboxManager::build("r", by_index, "/w", 0)
            .unwrap()
            .map(Arc::new);
        assert!(sb.is_some(), "namespace warn config yields a manager");
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), PathBuf::from("/w"), io.clone())
            .with_shell(sb, Duration::from_secs(5), Default::default());
        assert_eq!(host.shell("ls").unwrap(), "s");
        assert!(
            io.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("shell:"))
        );
    }

    #[test]
    fn real_read_file_success_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "data").unwrap();
        assert_eq!(RealScriptIo.read_file(&p).unwrap(), "data");
        let err = RealScriptIo
            .read_file(&dir.path().join("nope"))
            .unwrap_err();
        assert!(err.contains("read '"));
    }

    #[test]
    fn real_write_file_creates_parents_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path exercises the create_dir_all(Some(parent)) branch.
        let nested = dir.path().join("sub/deep/out.txt");
        let msg = RealScriptIo.write_file(&nested, "body").unwrap();
        assert!(msg.contains("wrote 4 bytes"), "got: {msg}");
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "body");
    }

    #[test]
    fn real_write_file_create_dir_error() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where a parent directory is expected → create_dir_all fails.
        let blocker = dir.path().join("afile");
        std::fs::write(&blocker, "x").unwrap();
        let err = RealScriptIo
            .write_file(&blocker.join("child.txt"), "b")
            .unwrap_err();
        assert!(err.contains("create dir"), "got: {err}");
    }

    #[test]
    fn real_write_file_write_error() {
        let dir = tempfile::tempdir().unwrap();
        // The path itself is an existing directory → std::fs::write fails.
        let err = RealScriptIo.write_file(dir.path(), "b").unwrap_err();
        assert!(err.contains("write '"), "got: {err}");
    }

    #[test]
    fn real_write_file_parentless_path() {
        // An empty path has no parent → the `if let Some(parent)` None arm is
        // taken (no dir creation), then the write itself fails.
        let err = RealScriptIo.write_file(Path::new(""), "b").unwrap_err();
        assert!(err.contains("write '"), "got: {err}");
    }

    #[test]
    fn real_env_var_set_and_unset() {
        temp_env::with_var("LEVIATH_SCRIPT_TEST", Some("v"), || {
            assert_eq!(RealScriptIo.env_var("LEVIATH_SCRIPT_TEST").unwrap(), "v");
        });
        temp_env::with_var_unset("LEVIATH_SCRIPT_TEST_UNSET", || {
            assert!(
                RealScriptIo
                    .env_var("LEVIATH_SCRIPT_TEST_UNSET")
                    .unwrap_err()
                    .contains("not set")
            );
        });
    }

    #[test]
    fn default_shell_is_platform_appropriate() {
        let (shell, flag) = default_shell();
        assert!(!shell.is_empty());
        assert!(!flag.is_empty());
    }

    /// Both answers, from whichever platform is running the test. A script tool
    /// gets `/bin/sh` everywhere it exists and `cmd.exe` where it does not -
    /// never the operator's `$SHELL`, which is what makes a Rhai tool behave
    /// the same on every machine.
    #[test]
    fn default_shell_for_answers_per_platform() {
        assert_eq!(default_shell_for("windows"), ("cmd.exe", "/C"));
        for posix in ["linux", "macos", "freebsd", "haiku"] {
            assert_eq!(default_shell_for(posix), ("/bin/sh", "-c"), "{posix}");
        }
    }

    #[test]
    fn new_wires_real_io() {
        // Construction path for the real backend (Arc<RealScriptIo>).
        let host = DaemonScriptHost::new(all_allowed(), std::env::temp_dir());
        // env_var goes through RealScriptIo; a guaranteed-unset var errors.
        temp_env::with_var_unset("LEVIATH_DEFINITELY_UNSET_XYZ", || {
            assert!(host.env_var("LEVIATH_DEFINITELY_UNSET_XYZ").is_err());
        });
    }

    /// A script's writes spend the run's budget: `write_file` is refused over
    /// the ceiling before it lands, and a redirect in `shell` is charged after.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_scripts_writes_spend_the_run_budget() {
        let dir = tempfile::tempdir().unwrap();
        let writes = Arc::new(crate::daemon::tool_service::WriteBudget::with_probe(
            leviath_core::write_limits::WriteLimits {
                per_call: Some(4),
                per_run: None,
            },
            |_| Some(leviath_core::write_limits::MIN_FREE_BYTES * 100),
        ));
        let allow = ScriptAllow {
            http_get: false,
            http_post: false,
            shell: true,
            read_file: false,
            write_file: true,
            env_var: false,
        };
        let host = DaemonScriptHost::new(allow, dir.path().to_path_buf())
            .with_write_budget(writes.clone());
        host.write_file("small.txt", "abc").expect("fits");
        assert_eq!(writes.written(), 3);
        let err = host
            .write_file("big.txt", "too big")
            .expect_err("over the per-call ceiling");
        assert!(err.contains("per-call"), "{err}");
        assert!(!dir.path().join("big.txt").exists());
        // The redirect's bytes are counted once the shell has run.
        // The real shell blocks on the runtime, so it runs off the async
        // thread, the way the tool lane's `block_in_place` places it.
        tokio::task::spawn_blocking(move || host.shell("echo hi > out.txt"))
            .await
            .expect("joined")
            .expect("ran");
        let written = writes.written();
        assert!(written > 3, "{written}");
    }

    #[test]
    fn cap_script_io_leaves_small_strings_untouched() {
        let s = "small".to_string();
        assert_eq!(cap_script_io(s.clone()), s);
    }

    #[test]
    fn cap_script_io_truncates_oversized_strings_below_the_rhai_limit() {
        let big = "x".repeat(MAX_SCRIPT_IO_BYTES + 5_000);
        let capped = cap_script_io(big);
        assert!(capped.len() < 1_000_000, "must stay under the 1MB Rhai cap");
        assert!(capped.contains("[...truncated by leviath"));
    }

    #[test]
    fn cap_script_io_truncates_on_a_char_boundary() {
        // A multi-byte char straddling the cap must not be split mid-codepoint.
        let mut s = "a".repeat(MAX_SCRIPT_IO_BYTES - 1);
        s.push('é'); // 2 bytes, crossing the boundary
        s.push_str(&"b".repeat(10));
        let capped = cap_script_io(s);
        // Valid UTF-8 (would panic on construction if a codepoint were split).
        assert!(capped.contains("[...truncated by leviath"));
    }
}
