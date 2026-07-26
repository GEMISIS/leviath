//! The daemon's real [`ScriptHost`] for Rhai script tools (issue #97, Layer 3).
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
//! vars) — the same approach the MCP and package-registry tests use.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
/// unrecognized value yields `None` (the field is left at the global default) —
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

/// The effective `[tool_script_permissions]` for an agent: the global config with
/// the agent's own blueprint `[tool_script_permissions]` overlaid per field (a
/// field the agent sets wins; an unset or unrecognized field keeps the global
/// value). Parsed CLI-side (these types live in the CLI config, not
/// `leviath-core`), mirroring `parse_blueprint_mcp_servers`. Since agents can
/// ship their own tool scripts, they can also tighten/loosen the per-function
/// permissions for their own run.
pub fn effective_script_permissions(
    global: &ScriptToolPermissions,
    manifest_toml: &str,
) -> ScriptToolPermissions {
    let mut eff = global.clone();
    let Ok(value) = manifest_toml.parse::<toml::Value>() else {
        return eff;
    };
    let Some(table) = value
        .get("tool_script_permissions")
        .and_then(|v| v.as_table())
    else {
        return eff;
    };
    // For each key the agent set to a recognized value, override that field.
    let apply = |key: &str, slot: &mut ScriptPermission| {
        if let Some(p) = table
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(parse_script_permission_str)
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
    /// exactly like the built-in `shell` tool — a script can't escape the
    /// isolation the agent's stage declared. `None` runs on the host.
    sandbox: Option<Arc<SandboxManager>>,
    /// Wall-clock cap on a single `shell()` call, so a runaway command can't hang
    /// the agent (mirrors the built-in shell tool's timeout).
    shell_timeout: Duration,
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
        }
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
    ) -> Self {
        self.sandbox = sandbox;
        self.shell_timeout = shell_timeout;
        self
    }

    /// Resolve a script-supplied `read_file` path against the workdir, rejecting
    /// any `..` escape (mirrors `BuiltinTools::resolve`).
    fn resolve_in_workdir(&self, requested: &str) -> Result<PathBuf, String> {
        let raw = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.workdir.join(requested)
        };
        let mut normalized = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(format!("path '{requested}' escapes the working directory"));
                    }
                }
                c => normalized.push(c),
            }
        }
        if !normalized.starts_with(&self.workdir) {
            return Err(format!(
                "path '{requested}' would escape the working directory"
            ));
        }
        Ok(normalized)
    }
}

/// The standard `[denied]` message for a host function blocked by
/// `[tool_script_permissions]`.
fn denied(func: &str) -> String {
    format!("[denied] script host function '{func}' is denied by tool_script_permissions")
}

impl ScriptHost for DaemonScriptHost {
    fn http_get(&self, url: &str, headers: BTreeMap<String, String>) -> Result<String, String> {
        if !self.allow.http_get {
            return Err(denied("http_get"));
        }
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
        self.io.http_post(url, body, headers)
    }

    fn shell(&self, command: &str) -> Result<String, String> {
        if !self.allow.shell {
            return Err(denied("shell"));
        }
        let (shell, flag) = default_shell();
        // With a sandbox, build the command that runs inside the current stage's
        // container / namespace; otherwise run the shell directly on the host
        // (both target the agent workdir). Same routing as the built-in shell tool.
        let cmd = match &self.sandbox {
            Some(sb) => sb.build_command(shell, flag, command, &self.workdir),
            None => host_shell_command(shell, flag, command, &self.workdir),
        };
        self.io.run_shell(cmd, self.shell_timeout)
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
        let resolved = self.resolve_in_workdir(path)?;
        self.io.write_file(&resolved, content)
    }

    fn env_var(&self, name: &str) -> Result<String, String> {
        if !self.allow.env_var {
            return Err(denied("env_var"));
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

impl RealScriptIo {
    /// Build a short-timeout blocking HTTP client for a single request. The
    /// builder only fails on TLS-backend init, which never happens in practice.
    /// If it ever does panic, `leviath_scripting::execute`'s `catch_unwind`
    /// catches it cleanly (issue #109).
    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build blocking reqwest client")
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
    fn send(req: reqwest::blocking::RequestBuilder) -> Result<String, String> {
        let resp = req.send().map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        let text = cap_script_io(resp.text().map_err(|e| format!("read body: {e}"))?);
        if status.is_success() {
            Ok(text)
        } else {
            Err(format!("http {status}: {text}"))
        }
    }
}

/// Cap a host-I/O string below the tool engine's 1 MB `max_string_size`
/// (`build_tool_engine`) so an oversized fetch/read/shell result can't raise the
/// NON-CATCHABLE `ErrorDataTooLarge` inside a Rhai tool script (it aborts the tool
/// even inside try/catch). This is only a crash guard — context-size truncation is
/// handled downstream by region budgets and any in-script truncation.
const MAX_SCRIPT_IO_BYTES: usize = 900_000;

fn cap_script_io(mut s: String) -> String {
    if s.len() > MAX_SCRIPT_IO_BYTES {
        let mut end = MAX_SCRIPT_IO_BYTES;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n[...truncated by leviath: response exceeded 900 KB]");
    }
    s
}

impl ScriptIo for RealScriptIo {
    fn http_get(&self, url: &str, headers: BTreeMap<String, String>) -> Result<String, String> {
        let client = Self::client();
        Self::send(Self::with_headers(client.get(url), headers))
    }

    fn http_post(
        &self,
        url: &str,
        body: &str,
        headers: BTreeMap<String, String>,
    ) -> Result<String, String> {
        let client = Self::client();
        Self::send(Self::with_headers(
            client.post(url).body(body.to_string()),
            headers,
        ))
    }

    fn run_shell(&self, mut cmd: TokioCommand, timeout: Duration) -> Result<String, String> {
        // The script engine drives this from a `spawn_blocking` thread (not a
        // runtime worker), so blocking on the current runtime is safe here and
        // lets us reuse tokio's timeout — the same mechanism the built-in shell
        // tool uses.
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async move {
            match tokio::time::timeout(timeout, cmd.output()).await {
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
fn default_shell() -> (&'static str, &'static str) {
    #[cfg(windows)]
    {
        ("cmd.exe", "/C")
    }
    #[cfg(not(windows))]
    {
        ("/bin/sh", "-c")
    }
}

/// Build the host (un-sandboxed) shell command pointed at `workdir` — the
/// no-sandbox arm of [`DaemonScriptHost::shell`].
fn host_shell_command(shell: &str, flag: &str, command: &str, workdir: &Path) -> TokioCommand {
    let mut c = TokioCommand::new(shell);
    c.arg(flag).arg(command).current_dir(workdir);
    c
}

/// Combine a finished command's stdout and (non-empty) stderr into one string,
/// preserving the prior `shell()` contract.
fn combine_shell_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut out = String::from_utf8_lossy(stdout).into_owned();
    let err = String::from_utf8_lossy(stderr);
    if !err.trim().is_empty() {
        out.push_str(&err);
    }
    out
}

#[cfg(test)]
mod tests {
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
    fn effective_perms_agent_overrides_win_per_field() {
        // Global denies everything; the agent's blueprint loosens/sets several
        // fields (exercising the allow/deny/inherit parse arms) and leaves the
        // rest at the global value.
        let global = perms(ScriptPermission::Deny);
        let manifest = "\
            [tool_script_permissions]\n\
            http_get = \"allow\"\n\
            shell = \"deny\"\n\
            write_file = \"inherit\"\n";
        let eff = effective_script_permissions(&global, manifest);
        assert_eq!(eff.http_get, ScriptPermission::Allow, "allow arm");
        assert_eq!(eff.shell, ScriptPermission::Deny, "deny arm");
        assert_eq!(eff.write_file, ScriptPermission::Inherit, "inherit arm");
        assert_eq!(eff.env_var, ScriptPermission::Deny, "unset keeps global");
        assert_eq!(eff.read_file, ScriptPermission::Deny);
        assert_eq!(eff.http_post, ScriptPermission::Deny);
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

    #[test]
    fn allowed_calls_delegate_to_io() {
        let io = RecordingIo::arc();
        let host = DaemonScriptHost::with_io(all_allowed(), std::env::temp_dir(), io.clone());
        assert_eq!(host.http_get("http://x", BTreeMap::new()).unwrap(), "g");
        assert_eq!(
            host.http_post("http://x", "b", BTreeMap::new()).unwrap(),
            "p"
        );
        assert_eq!(host.shell("ls").unwrap(), "s");
        assert_eq!(host.write_file("out.txt", "body").unwrap(), "w");
        assert_eq!(host.env_var("HOME").unwrap(), "e");
        let calls = io.calls.lock().unwrap().clone();
        assert!(calls.contains(&"get:http://x".to_string()));
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
        // absolute on Windows — it needs a drive/UNC prefix), and outside the
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
            );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
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
    /// closes — so `resp.text()` errors on the incomplete body (mirrors the
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
            .with_shell(sb, Duration::from_secs(5));
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

    #[test]
    fn new_wires_real_io() {
        // Construction path for the real backend (Arc<RealScriptIo>).
        let host = DaemonScriptHost::new(all_allowed(), std::env::temp_dir());
        // env_var goes through RealScriptIo; a guaranteed-unset var errors.
        temp_env::with_var_unset("LEVIATH_DEFINITELY_UNSET_XYZ", || {
            assert!(host.env_var("LEVIATH_DEFINITELY_UNSET_XYZ").is_err());
        });
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
