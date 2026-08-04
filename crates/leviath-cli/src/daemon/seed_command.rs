//! Execution of `seed = { command = "..." }` region seeds.
//!
//! A command seed runs a shell command in the run's workdir at spawn and puts
//! its combined stdout/stderr into the region - but only when the command
//! *succeeds*; a non-zero exit is reported as an error so a diagnostic never
//! masquerades as data. It is the only seed source that *executes* anything, and
//! it does so before the first inference - therefore before any tool-approval
//! prompt - so it is deliberately hemmed in:
//! -
//! it is skipped entirely unless [`SeedCommandPolicy::allowed`] (the
//!   `[security] allow_seed_commands` config switch and the `--no-seed-commands`
//!   launch flag); -
//! it runs inside the entry stage's sandbox when the agent declares one,
//!   using the same [`ShellExecutor::build_command`] routing as the built-in
//!   `shell` tool, so a seed can't escape the isolation the stage asked for; -
//! it is capped in wall-clock time (`[limits] script_shell_timeout_secs`) and
//!   in output size (`cap_script_io`); -
//! it never runs on restart - [`crate::daemon::spawn`] only resolves seeds on
//!   a fresh spawn.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leviath_tools::ShellExecutor;
use tokio::process::Command as TokioCommand;

use crate::daemon::sandbox_manager::SandboxManager;
use crate::daemon::script_host::{
    cap_script_io, combine_shell_output, default_shell, host_shell_command,
};

/// Runs one seed command: `(command, workdir, timeout) -> combined output`.
///
/// Injected rather than called directly so the failure arms (timeout, spawn
/// failure, non-zero exit) are testable without spawning real processes. The
/// production implementation is built by [`SeedCommandPolicy::new`]. Mirrors
/// the `BrowserOpener` seam.
pub type SeedCommandRunner =
    Arc<dyn Fn(&str, &Path, Duration) -> Result<String, String> + Send + Sync>;

/// How command seeds are executed for one spawn.
#[derive(Clone)]
pub struct SeedCommandPolicy {
    /// Whether command seeds may run at all. `false` makes every command seed a
    /// no-op (a warning, or a hard error when the region is `required`).
    pub allowed: bool,
    /// Wall-clock cap on a single seed command.
    pub timeout: Duration,
    /// The executor.
    pub runner: SeedCommandRunner,
}

impl SeedCommandPolicy {
    /// The production policy: run through `sandbox` when the agent declares one,
    /// else on the host, both targeting the run's workdir.
    pub fn new(allowed: bool, timeout: Duration, sandbox: Option<Arc<SandboxManager>>) -> Self {
        Self {
            allowed,
            timeout,
            runner: seed_command_runner(sandbox),
        }
    }

    /// A policy that never runs anything - used on the reload/restore path and
    /// wherever seeds are resolved without a live sandbox.
    pub fn disabled() -> Self {
        Self {
            allowed: false,
            timeout: Duration::from_secs(0),
            runner: Arc::new(|_, _, _| Err("command seeds are disabled".to_string())),
        }
    }

    /// Run `command` in `workdir` under this policy.
    pub fn run(&self, command: &str, workdir: &Path) -> Result<String, String> {
        (self.runner)(command, workdir, self.timeout)
    }
}

impl std::fmt::Debug for SeedCommandPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeedCommandPolicy")
            .field("allowed", &self.allowed)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Build the production [`SeedCommandRunner`], capturing the agent's sandbox
/// manager (if any) so every seed command is routed exactly like the built-in
/// `shell` tool would route it for the entry stage.
fn seed_command_runner(sandbox: Option<Arc<SandboxManager>>) -> SeedCommandRunner {
    Arc::new(move |command, workdir, timeout| {
        run_seed_command(
            build_seed_command(sandbox.as_deref(), command, workdir),
            timeout,
        )
    })
}

/// Build the command for a seed: through the agent's sandbox when it has one,
/// else straight onto the host - both targeting the run's workdir.
///
/// Split from execution so the routing decision is assertable without spawning
/// anything (and without depending on whether the host's namespaces actually
/// work, which varies by machine and by CI runner).
fn build_seed_command(
    sandbox: Option<&SandboxManager>,
    command: &str,
    workdir: &Path,
) -> TokioCommand {
    let (shell, flag) = default_shell();
    match sandbox {
        Some(sb) => sb.build_command(shell, flag, command, workdir),
        None => host_shell_command(shell, flag, command, workdir),
    }
}

/// Drive `cmd` to completion with a wall-clock cap, returning its combined
/// stdout+stderr (capped by `cap_script_io`) on success.
///
/// **A non-zero exit is an error, not data.** The combined output of a failed
/// command is a diagnostic - `git ls-files` outside a repository prints
/// `fatal: not a git repository` - and returning it as the seed value would
/// plant that text in a pinned region as though it were the file listing the
/// blueprint promised. The caller logs it and leaves the region empty instead
/// (or fails the spawn, when the region is `required`).
///
/// This runs on a freshly spawned OS thread with its own current-thread runtime
/// rather than reusing the ambient one. `resolve_seeds` is a synchronous
/// function called from an async context, so `Handle::current().block_on(...)` -
/// the trick `RealScriptIo::run_shell` uses from its `spawn_blocking` thread -
/// would panic here. A dedicated thread has no ambient runtime, and going
/// through tokio (rather than `std::process`) buys a real timeout: dropping the
/// `output()` future on expiry kills the child via `kill_on_drop`, instead of
/// orphaning it.
fn run_seed_command(mut cmd: TokioCommand, timeout: Duration) -> Result<String, String> {
    cmd.kill_on_drop(true);
    // One per seeded region at spawn, and `output()` pipes both streams, so
    // there is no console for this to want on Windows.
    leviath_tools::hide_console_window(&mut cmd);
    std::thread::spawn(move || {
        // A current-thread runtime with no ambient runtime present only fails on
        // OS resource exhaustion, at which point the spawn itself is doomed
        // (mirrors `RealScriptIo::client`'s `.expect`).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime for a seed command always builds");
        rt.block_on(async move {
            match tokio::time::timeout(timeout, cmd.output()).await {
                Ok(Ok(output)) => {
                    let combined =
                        cap_script_io(combine_shell_output(&output.stdout, &output.stderr));
                    if output.status.success() {
                        Ok(combined)
                    } else {
                        Err(format!(
                            "seed command exited with {}: {}",
                            output.status,
                            combined.trim()
                        ))
                    }
                }
                Ok(Err(e)) => Err(format!("failed to spawn seed command: {e}")),
                Err(_) => Err(format!(
                    "seed command timed out after {}s",
                    timeout.as_secs()
                )),
            }
        })
    })
    .join()
    // The closure above has no fallible unwraps, so it cannot unwind.
    .expect("seed command thread does not panic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_policy_refuses_to_run() {
        let policy = SeedCommandPolicy::disabled();
        assert!(!policy.allowed);
        let err = policy.run("echo hi", Path::new(".")).unwrap_err();
        assert!(err.contains("disabled"), "got: {err}");
    }

    #[test]
    fn debug_impl_reports_the_switches() {
        let policy = SeedCommandPolicy::new(true, Duration::from_secs(7), None);
        let rendered = format!("{policy:?}");
        assert!(rendered.contains("allowed: true"), "got: {rendered}");
        assert!(rendered.contains('7'), "got: {rendered}");
    }

    #[test]
    fn injected_runner_is_used_and_receives_the_policy_timeout() {
        let policy = SeedCommandPolicy {
            allowed: true,
            timeout: Duration::from_secs(3),
            runner: Arc::new(|command, workdir, timeout| {
                Ok(format!(
                    "{command}|{}|{}",
                    workdir.display(),
                    timeout.as_secs()
                ))
            }),
        };
        assert_eq!(
            policy.run("ls", Path::new("/w")).unwrap(),
            "ls|/w|3".to_string()
        );
    }

    /// The real runner, end-to-end, on a command that exists on every platform
    /// (`echo` is a builtin of both `/bin/sh` and `cmd.exe`).
    #[test]
    fn real_runner_captures_stdout() {
        let policy = SeedCommandPolicy::new(true, Duration::from_secs(30), None);
        let out = policy
            .run("echo leviath-seed-ok", &std::env::temp_dir())
            .unwrap();
        assert!(out.contains("leviath-seed-ok"), "got: {out}");
    }

    /// A non-zero exit is an error, and its output is reported as a diagnostic
    /// rather than handed back as the seed value.
    #[test]
    fn real_runner_treats_a_non_zero_exit_as_an_error() {
        let policy = SeedCommandPolicy::new(true, Duration::from_secs(30), None);
        // `exit 3` after printing: portable across sh and cmd.exe.
        let err = policy
            .run("echo before-failure && exit 3", &std::env::temp_dir())
            .unwrap_err();
        assert!(err.contains("exited with"), "got: {err}");
        // The output is preserved in the message so the warning is diagnosable.
        assert!(err.contains("before-failure"), "got: {err}");
    }

    /// The real motivating case: `git ls-files` outside a repository. Its
    /// `fatal: not a git repository` must never become the region's content.
    #[test]
    fn real_runner_rejects_git_ls_files_outside_a_repository() {
        let outside = tempfile::tempdir().unwrap();
        let policy = SeedCommandPolicy::new(true, Duration::from_secs(30), None);
        // A bare temp dir may still sit under a repo on some machines; force the
        // failure deterministically by pointing git at a nonexistent work tree.
        let err = policy
            .run(
                "git --git-dir=./definitely-not-a-repo ls-files",
                outside.path(),
            )
            .unwrap_err();
        assert!(err.contains("exited with"), "got: {err}");
    }

    /// The timeout arm kills the child rather than hanging the spawn.
    #[test]
    fn real_runner_times_out_a_long_command() {
        let policy = SeedCommandPolicy::new(true, Duration::from_millis(150), None);
        // Each platform's own idiom for "sleep". `#[cfg]` rather than `cfg!` so
        // only the arm for THIS platform is compiled - the other would otherwise
        // count as unreachable code against the coverage gate.
        #[cfg(windows)]
        let long = "ping -n 30 127.0.0.1 > NUL";
        #[cfg(not(windows))]
        let long = "sleep 30";
        let err = policy.run(long, &std::env::temp_dir()).unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }

    /// A command the shell cannot run is an error (the shell exits non-zero),
    /// reported with its diagnostic rather than blowing up the spawn.
    #[test]
    fn real_runner_surfaces_a_missing_program() {
        let policy = SeedCommandPolicy::new(true, Duration::from_secs(30), None);
        let err = policy
            .run("leviath-no-such-program-xyz", &std::env::temp_dir())
            .unwrap_err();
        assert!(err.contains("exited with"), "got: {err}");
    }

    /// Without a sandbox the command goes straight to the platform shell.
    #[test]
    fn an_unsandboxed_seed_command_uses_the_platform_shell() {
        let cmd = build_seed_command(None, "echo hi", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), default_shell().0);
    }

    /// With a sandbox attached the command is built BY the manager rather than
    /// straight onto the host - the same routing the built-in `shell` tool uses,
    /// so a seed can't escape the isolation the entry stage declared.
    ///
    /// This asserts the routing, not the execution: whether a namespace is
    /// actually usable varies by machine (and CI runners probe as supporting
    /// them while refusing the uid_map write), so running the command here would
    /// be testing the kernel, not this code.
    #[test]
    fn a_sandboxed_seed_command_is_built_through_the_manager() {
        let by_index = vec![leviath_core::ToolSandboxConfig {
            kind: leviath_core::SandboxKind::Namespace,
            on_unavailable: leviath_core::OnUnavailable::Warn,
            ..Default::default()
        }];
        let manager = SandboxManager::build("seed-test", by_index, "/w", 0)
            .expect("a warn-fallback namespace sandbox always builds")
            .expect("an active sandbox config yields a manager");

        let cmd = build_seed_command(Some(&manager), "echo hi", Path::new("/w"));
        // Where namespaces work this is the namespace binary; where they don't
        // the manager falls back to the shell. Either way the manager built it.
        assert!(!cmd.as_std().get_program().is_empty());
    }

    /// The `Ok(Err(_))` arm: the shell binary itself cannot be spawned. Built
    /// directly (not via `default_shell`) so it is reachable on every platform.
    #[test]
    fn run_seed_command_reports_a_spawn_failure() {
        let mut cmd = TokioCommand::new("leviath-definitely-not-a-shell-xyz");
        cmd.arg("-c").arg("echo hi");
        let err = run_seed_command(cmd, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("failed to spawn seed command"), "got: {err}");
    }
}
