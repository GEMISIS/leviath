//! Per-agent sandbox lifecycle and shell-command routing.
//!
//! A [`SandboxManager`] owns every sandbox an agent needs: it creates the
//! containers its stages call for **eagerly at spawn** (blocking `docker run`,
//! keyed and deduplicated by config signature so identical configs across stages
//! share one warm container), routes each shell call to the *current* stage's
//! sandbox, and tears every container down at reap. `namespace` and `none` kinds
//! need no persistent state - they are pure per-exec command wrapping.
//!
//! It implements [`leviath_tools::ShellExecutor`], so the built-in shell tool
//! runs its command inside the sandbox transparently; file tools stay on the
//! host over the bind-mounted workdir. The create/dedup/error logic is factored
//! behind an injected command-runner (`build_with`) so it is unit-testable
//! with no container runtime installed.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Mutex as StdMutex, PoisonError};

use leviath_core::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};
use leviath_sys::ContainerRunSpec;
use leviath_tools::ShellExecutor;
use tokio::process::Command as TokioCommand;

/// POSIX shell used *inside* a container. Every image ships `/bin/sh`, whereas
/// the host's detected shell (e.g. `/bin/zsh` on macOS) generally isn't present
/// in the image - so container exec must use its own shell, not the host's.
const CONTAINER_SHELL: &str = "sh";
const CONTAINER_SHELL_FLAG: &str = "-c";

/// Runs a sandbox lifecycle command (`docker run` / `docker rm`), returning the
/// stderr text on failure. Injected so [`SandboxManager::build_with`] is testable
/// without a real runtime.
type CmdRunner<'a> = dyn Fn(&[String]) -> Result<(), String> + 'a;

/// A container we started and are responsible for removing.
#[derive(Debug, Clone)]
struct LiveContainer {
    /// The engine binary that started it (so teardown uses the same one).
    engine: String,
    name: String,
}

/// Owns an agent's sandboxes for its whole lifetime. Created at spawn, updated
/// per stage via [`Self::set_stage`], torn down at reap via [`Self::destroy_all`].
#[derive(Debug)]
pub struct SandboxManager {
    /// Live containers keyed by config signature; immutable after construction.
    containers: HashMap<u64, LiveContainer>,
    /// Per-stage-index resolved sandbox config; immutable after construction.
    by_index: Vec<ToolSandboxConfig>,
    /// Whether Linux namespaces are usable on this host - captured at build so
    /// `build_command` (the `ShellExecutor` hot path) doesn't re-probe and both
    /// its namespace arms are reachable regardless of the test platform.
    namespace_ok: bool,
    /// The current stage's config - the only mutable state, swapped on stage
    /// change (the manager lives behind an `Arc`, so interior mutability).
    current: StdMutex<ToolSandboxConfig>,
}

/// Signature identifying a distinct container: same engine + image + network +
/// mounts → one shared warm container. (Deterministic within a process -
/// `DefaultHasher` uses fixed keys.)
fn signature(cfg: &ToolSandboxConfig) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cfg.engine.hash(&mut h);
    cfg.image.hash(&mut h);
    cfg.network.hash(&mut h);
    cfg.mounts.hash(&mut h);
    h.finish()
}

/// Reduce a run id to a Docker-safe name fragment (`[a-zA-Z0-9_.-]`).
fn sanitize(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl SandboxManager {
    /// Build the manager for an agent whose stages resolve (in order) to
    /// `by_index`, bind-mounting `workdir`. `entry_index` selects the initial
    /// stage's config. Returns `Ok(None)` when nothing is sandboxed (the common
    /// case - the caller then attaches no executor and shell runs on the host).
    /// Returns `Err` when a required runtime is unavailable and that config's
    /// `on_unavailable` is `Error`.
    pub fn build(
        run_id: &str,
        by_index: Vec<ToolSandboxConfig>,
        workdir: &str,
        entry_index: usize,
    ) -> Result<Option<Self>, String> {
        // Auto-detected default engine, used only when a container config doesn't
        // name its own. Detection runs only if some stage actually needs a
        // container.
        let needs_container = by_index.iter().any(|c| c.kind == SandboxKind::Container);
        let detected = needs_container
            .then(leviath_sys::detect_container_engine)
            .flatten();
        let namespace_ok = leviath_sys::namespace_supported();
        Self::build_with(
            run_id,
            by_index,
            workdir,
            entry_index,
            detected,
            namespace_ok,
            &real_run,
        )
    }

    /// Testable core of [`Self::build`]: `detected` is the auto-detected engine
    /// (a config's own `engine` overrides it), `namespace_ok` reports namespace
    /// availability, and `run` executes the lifecycle commands.
    fn build_with(
        run_id: &str,
        by_index: Vec<ToolSandboxConfig>,
        workdir: &str,
        entry_index: usize,
        detected: Option<String>,
        namespace_ok: bool,
        run: &CmdRunner,
    ) -> Result<Option<Self>, String> {
        // Fast path: no stage isolates anything → no executor, zero overhead.
        if by_index.iter().all(|c| !c.is_active()) {
            return Ok(None);
        }

        let mut containers: HashMap<u64, LiveContainer> = HashMap::new();
        for cfg in &by_index {
            match cfg.kind {
                SandboxKind::None => {}
                SandboxKind::Namespace => {
                    if !namespace_ok {
                        unavailable(
                            cfg,
                            "namespace sandbox requires Linux (unshare); this host lacks it",
                            &containers,
                            run,
                        )?;
                    }
                }
                SandboxKind::Container => {
                    let sig = signature(cfg);
                    if containers.contains_key(&sig) {
                        continue; // identical config already has a warm container
                    }
                    // The config's own `engine` wins; else the auto-detected one.
                    let Some(engine) = cfg.engine.clone().or_else(|| detected.clone()) else {
                        unavailable(
                            cfg,
                            "no container engine found - install docker or podman, \
                             or set `engine` in [sandbox]",
                            &containers,
                            run,
                        )?;
                        continue;
                    };
                    let Some(image) = cfg.image.as_deref() else {
                        unavailable(
                            cfg,
                            "container sandbox requires an `image`",
                            &containers,
                            run,
                        )?;
                        continue;
                    };
                    let name = format!("leviath-{}-{:016x}", sanitize(run_id), sig);
                    let spec = ContainerRunSpec {
                        engine: &engine,
                        image,
                        workdir,
                        network: cfg.network,
                        mounts: &cfg.mounts,
                        name: &name,
                    };
                    match run(&leviath_sys::container_run_argv(&spec)) {
                        Ok(()) => {
                            containers.insert(sig, LiveContainer { engine, name });
                        }
                        Err(stderr) => {
                            let msg = format!(
                                "failed to start container '{image}' via '{engine}': {stderr}"
                            );
                            unavailable(cfg, &msg, &containers, run)?;
                        }
                    }
                }
            }
        }

        let current = by_index.get(entry_index).cloned().unwrap_or_default();
        Ok(Some(Self {
            containers,
            by_index,
            namespace_ok,
            current: StdMutex::new(current),
        }))
    }

    /// Point the shell tool at the sandbox for stage `index` (called by the tool
    /// service's `sync_stage` on every stage change).
    pub fn set_stage(&self, index: usize) {
        if let Some(cfg) = self.by_index.get(index) {
            *self.current.lock().unwrap_or_else(PoisonError::into_inner) = cfg.clone();
        }
    }

    /// Force-remove every container this manager started (best-effort). Called
    /// once, at reap, before the agent entity is despawned.
    pub fn destroy_all(&self) {
        self.destroy_with(&real_run);
    }

    /// Testable core of [`Self::destroy_all`].
    fn destroy_with(&self, run: &CmdRunner) {
        for c in self.containers.values() {
            let _ = run(&leviath_sys::container_rm_argv(&c.engine, &c.name));
        }
    }
}

/// Apply a config's `on_unavailable` policy: `Error` fails the build (after
/// tearing down anything already created so no container leaks); `Warn` logs and
/// lets the caller fall back to host execution.
fn unavailable(
    cfg: &ToolSandboxConfig,
    reason: &str,
    created: &HashMap<u64, LiveContainer>,
    run: &CmdRunner,
) -> Result<(), String> {
    match cfg.on_unavailable {
        OnUnavailable::Error => {
            for c in created.values() {
                let _ = run(&leviath_sys::container_rm_argv(&c.engine, &c.name));
            }
            Err(format!("sandbox unavailable: {reason}"))
        }
        OnUnavailable::Warn => {
            tracing::warn!("sandbox unavailable ({reason}); falling back to host execution");
            Ok(())
        }
    }
}

/// Real lifecycle-command runner: spawn synchronously, map a non-zero exit to
/// its stderr.
fn real_run(argv: &[String]) -> Result<(), String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("empty sandbox command".to_string());
    };
    // `docker run`/`docker rm` are bookkeeping the operator never watches, so
    // they get no console window on Windows - which is what `child_command`
    // arranges and a bare `Command::new` did not.
    let mut cmd = leviath_sys::child_command(program);
    cmd.args(args);
    let output = cmd.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

impl ShellExecutor for SandboxManager {
    fn build_command(
        &self,
        shell: &str,
        flag: &str,
        command: &str,
        workdir: &Path,
    ) -> TokioCommand {
        let cfg = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match cfg.kind {
            SandboxKind::None => {
                crate::daemon::script_host::host_shell_command(shell, flag, command, workdir)
            }
            SandboxKind::Namespace => {
                if self.namespace_ok {
                    let argv = leviath_sys::namespace_argv(shell, flag, command, cfg.network);
                    let mut c = leviath_sys::child_command_async(&argv[0]);
                    c.args(&argv[1..]).current_dir(workdir);
                    c
                } else {
                    // Warn-fallback build kept the manager alive without a usable
                    // namespace; run on the host.
                    crate::daemon::script_host::host_shell_command(shell, flag, command, workdir)
                }
            }
            SandboxKind::Container => match self.containers.get(&signature(&cfg)) {
                Some(lc) => {
                    let wd = workdir.to_string_lossy();
                    // Use the container's own shell (`sh`), not the host-detected
                    // one whose absolute path may not exist in the image.
                    let argv = leviath_sys::container_exec_argv(
                        &lc.engine,
                        &lc.name,
                        &wd,
                        CONTAINER_SHELL,
                        CONTAINER_SHELL_FLAG,
                        command,
                    );
                    let mut c = leviath_sys::child_command_async(&argv[0]);
                    c.args(&argv[1..]);
                    c
                }
                // Warn-fallback: the container was never created; run on the host.
                None => {
                    crate::daemon::script_host::host_shell_command(shell, flag, command, workdir)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container_cfg(
        image: &str,
        network: bool,
        on_unavailable: OnUnavailable,
    ) -> ToolSandboxConfig {
        ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: Some(image.to_string()),
            network,
            on_unavailable,
            ..Default::default()
        }
    }

    fn ns_cfg(on_unavailable: OnUnavailable) -> ToolSandboxConfig {
        ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            on_unavailable,
            ..Default::default()
        }
    }

    /// A runner that records every argv it was asked to run and always succeeds.
    fn recording_runner(
        log: &std::sync::Mutex<Vec<Vec<String>>>,
    ) -> impl Fn(&[String]) -> Result<(), String> + '_ {
        move |argv| {
            log.lock().unwrap().push(argv.to_vec());
            Ok(())
        }
    }

    /// A no-op runner that always succeeds. Shared (rather than inline closures)
    /// so a single instantiation is covered by the tests that actually invoke it.
    fn ok_runner(_argv: &[String]) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn all_host_yields_no_manager() {
        let by_index = vec![ToolSandboxConfig::default(), ToolSandboxConfig::default()];
        let m = SandboxManager::build_with("run1", by_index, "/work", 0, None, true, &ok_runner)
            .unwrap();
        assert!(m.is_none());
    }

    #[test]
    fn config_engine_overrides_autodetect() {
        // Non-prescriptive: a config naming its own engine uses that binary even
        // when nothing is auto-detected (`detected = None`).
        let log = std::sync::Mutex::new(Vec::new());
        let cfg = ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: Some("alpine".to_string()),
            engine: Some("nerdctl".to_string()),
            on_unavailable: OnUnavailable::Error,
            ..Default::default()
        };
        let m = SandboxManager::build_with(
            "r",
            vec![cfg],
            "/w",
            0,
            None, // no engine auto-detected
            true,
            &recording_runner(&log),
        )
        .unwrap()
        .unwrap();
        assert_eq!(log.lock().unwrap()[0][0], "nerdctl");
        assert_eq!(m.containers.values().next().unwrap().engine, "nerdctl");
    }

    #[test]
    fn dedups_identical_container_configs() {
        let log = std::sync::Mutex::new(Vec::new());
        let cfg = container_cfg("ubuntu:24.04", true, OnUnavailable::Error);
        let by_index = vec![cfg.clone(), cfg.clone(), cfg];
        let m = SandboxManager::build_with(
            "run-1",
            by_index,
            "/work",
            0,
            Some("docker".to_string()),
            true,
            &recording_runner(&log),
        )
        .unwrap()
        .unwrap();
        // Three identical stages → one container created.
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(m.containers.len(), 1);
        let argv = &log.lock().unwrap()[0];
        assert_eq!(argv[0], "docker");
        assert!(argv.contains(&"ubuntu:24.04".to_string()));
    }

    #[test]
    fn distinct_container_configs_each_get_a_container() {
        // Uses `ok_runner` (invoked twice here), which also covers the shared
        // no-op runner referenced by the never-invoking error/host tests.
        let by_index = vec![
            container_cfg("ubuntu:24.04", true, OnUnavailable::Error),
            container_cfg("node:22-slim", false, OnUnavailable::Error),
        ];
        let m = SandboxManager::build_with(
            "r",
            by_index,
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &ok_runner,
        )
        .unwrap()
        .unwrap();
        assert_eq!(m.containers.len(), 2);
    }

    #[test]
    fn missing_engine_errors_by_default() {
        let by_index = vec![container_cfg("ubuntu:24.04", true, OnUnavailable::Error)];
        let err =
            SandboxManager::build_with("r", by_index, "/w", 0, None, true, &ok_runner).unwrap_err();
        assert!(err.contains("no container engine"));
    }

    #[test]
    fn missing_engine_warns_and_falls_back() {
        let by_index = vec![container_cfg("ubuntu:24.04", true, OnUnavailable::Warn)];
        let m = SandboxManager::build_with("r", by_index, "/w", 0, None, true, &ok_runner)
            .unwrap()
            .unwrap();
        // No container created; build_command falls back to host.
        assert!(m.containers.is_empty());
        let cmd = m.build_command("sh", "-c", "echo hi", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), "sh");
    }

    #[test]
    fn container_without_image_errors() {
        let cfg = ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: None,
            on_unavailable: OnUnavailable::Error,
            ..Default::default()
        };
        let err = SandboxManager::build_with(
            "r",
            vec![cfg],
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &ok_runner,
        )
        .unwrap_err();
        assert!(err.contains("requires an `image`"), "got: {err}");
    }

    #[test]
    fn container_without_image_warns_and_skips() {
        // Warn variant: the missing-image config is skipped (the `continue`
        // after the warn) and the manager is still built with no container.
        let cfg = ToolSandboxConfig {
            kind: SandboxKind::Container,
            image: None,
            on_unavailable: OnUnavailable::Warn,
            ..Default::default()
        };
        let m = SandboxManager::build_with(
            "r",
            vec![cfg],
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &ok_runner,
        )
        .unwrap()
        .unwrap();
        assert!(m.containers.is_empty());
    }

    #[test]
    fn build_command_host_arm_when_current_stage_is_none() {
        // Entry stage is host (None) while a later stage is sandboxed → the
        // manager exists, and build_command on the host stage runs on the host.
        let by_index = vec![
            ToolSandboxConfig::default(),
            container_cfg("ubuntu:24.04", true, OnUnavailable::Warn),
        ];
        let m = SandboxManager::build_with("r", by_index, "/w", 0, None, true, &ok_runner)
            .unwrap()
            .unwrap();
        let cmd = m.build_command("zsh", "-c", "echo hi", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), "zsh");
    }

    #[test]
    fn container_start_failure_errors() {
        let by_index = vec![container_cfg("bad:image", true, OnUnavailable::Error)];
        let err = SandboxManager::build_with(
            "r",
            by_index,
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &|_| Err("no such image".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("failed to start container"));
        assert!(err.contains("no such image"));
    }

    #[test]
    fn error_teardown_removes_already_created_containers() {
        // First stage's container starts fine; second fails → the first must be
        // torn down (a `rm` appears in the log) before the error propagates.
        let log = std::sync::Mutex::new(Vec::<Vec<String>>::new());
        let run = |argv: &[String]| -> Result<(), String> {
            log.lock().unwrap().push(argv.to_vec());
            // Fail only the second image's run.
            if argv.contains(&"node:22-slim".to_string()) {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        };
        let by_index = vec![
            container_cfg("ubuntu:24.04", true, OnUnavailable::Error),
            container_cfg("node:22-slim", true, OnUnavailable::Error),
        ];
        let err = SandboxManager::build_with(
            "r",
            by_index,
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &run,
        )
        .unwrap_err();
        assert!(err.contains("failed to start container"));
        let calls = log.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("docker")
                    && c.contains(&"rm".to_string()))
        );
    }

    #[test]
    fn namespace_unavailable_errors_by_default() {
        let by_index = vec![ns_cfg(OnUnavailable::Error)];
        let err = SandboxManager::build_with("r", by_index, "/w", 0, None, false, &ok_runner)
            .unwrap_err();
        assert!(err.contains("namespace"));
    }

    #[test]
    fn namespace_unavailable_warns_and_falls_back() {
        let by_index = vec![ns_cfg(OnUnavailable::Warn)];
        let m = SandboxManager::build_with("r", by_index, "/w", 0, None, false, &ok_runner)
            .unwrap()
            .unwrap();
        let cmd = m.build_command("sh", "-c", "echo hi", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), "sh");
    }

    #[test]
    fn set_stage_switches_current_config() {
        let by_index = vec![
            ToolSandboxConfig::default(), // stage 0: host
            container_cfg("ubuntu:24.04", true, OnUnavailable::Warn), // stage 1: container
        ];
        // Warn so no engine needed; stage 1's container just won't exist → host.
        let m = SandboxManager::build_with("r", by_index, "/w", 0, None, true, &ok_runner)
            .unwrap()
            .unwrap();
        assert_eq!(m.current.lock().unwrap().kind, SandboxKind::None);
        m.set_stage(1);
        assert_eq!(m.current.lock().unwrap().kind, SandboxKind::Container);
        m.set_stage(99); // out of range: no-op, no panic
        assert_eq!(m.current.lock().unwrap().kind, SandboxKind::Container);
    }

    #[test]
    fn build_command_container_uses_docker_exec() {
        let log = std::sync::Mutex::new(Vec::new());
        let by_index = vec![container_cfg("ubuntu:24.04", true, OnUnavailable::Error)];
        let m = SandboxManager::build_with(
            "r",
            by_index,
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &recording_runner(&log),
        )
        .unwrap()
        .unwrap();
        let cmd = m.build_command("sh", "-c", "ls", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), "docker");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"ls".to_string()));
    }

    #[test]
    fn build_command_namespace_uses_unshare_when_supported() {
        // `namespace_ok = true` is injected, so this exercises the `unshare` arm
        // on any platform (the manager captured the flag at build time).
        let by_index = vec![ns_cfg(OnUnavailable::Error)];
        let m = SandboxManager::build_with("r", by_index, "/w", 0, None, true, &ok_runner)
            .unwrap()
            .unwrap();
        let cmd = m.build_command("sh", "-c", "whoami", Path::new("/w"));
        assert_eq!(cmd.as_std().get_program(), "unshare");
    }

    #[test]
    fn destroy_all_removes_every_container() {
        let log = std::sync::Mutex::new(Vec::new());
        let by_index = vec![
            container_cfg("ubuntu:24.04", true, OnUnavailable::Error),
            container_cfg("node:22-slim", true, OnUnavailable::Error),
        ];
        let m = SandboxManager::build_with(
            "r",
            by_index,
            "/w",
            0,
            Some("docker".to_string()),
            true,
            &recording_runner(&log),
        )
        .unwrap()
        .unwrap();
        let rm_log = std::sync::Mutex::new(Vec::new());
        m.destroy_with(&recording_runner(&rm_log));
        let calls = rm_log.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c.contains(&"rm".to_string())));
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize("abc-123_x.y"), "abc-123_x-y");
        assert_eq!(sanitize("a/b:c d"), "a-b-c-d");
    }

    #[test]
    fn destroy_all_with_no_containers_is_a_noop() {
        // A namespace manager has no containers; destroy_all must run cleanly
        // (covers the real-runner entry point without invoking an engine).
        let m = SandboxManager::build_with(
            "r",
            vec![ns_cfg(OnUnavailable::Warn)],
            "/w",
            0,
            None,
            false,
            &ok_runner,
        )
        .unwrap()
        .unwrap();
        m.destroy_all();
        assert!(m.containers.is_empty());
    }

    // `real_run` actually spawns a process, so its three arms are covered with
    // trivial host commands. Split per-platform because there is no portable
    // shell/no-op binary (mirrors why `format_command_output` was split out).
    #[cfg(unix)]
    #[test]
    fn real_run_success_nonzero_and_spawn_error_unix() {
        assert!(real_run(&["true".to_string()]).is_ok());
        let err = real_run(&[
            "sh".to_string(),
            "-c".to_string(),
            "echo boom 1>&2; exit 1".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("boom"), "stderr should surface: {err}");
        assert!(real_run(&["leviath-no-such-binary-xyz".to_string()]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn real_run_success_nonzero_and_spawn_error_windows() {
        assert!(real_run(&["cmd".to_string(), "/C".to_string(), "exit 0".to_string()]).is_ok());
        assert!(real_run(&["cmd".to_string(), "/C".to_string(), "exit 1".to_string()]).is_err());
        assert!(real_run(&["leviath-no-such-binary-xyz".to_string()]).is_err());
    }

    // Live end-to-end verification against a real container engine is not a
    // compiled test here - an `#[ignore]`d test still counts as uncovered against
    // the crate's hard-100% coverage gate. To verify the real create → exec →
    // destroy path manually (with a container daemon running):
    //
    //   docker pull alpine:latest
    //   lev run <agent-with `[sandbox] kind="container" image="alpine"`> \
    //       --task "run `cat /etc/os-release` and report the OS" --yolo
    //
    // The agent's shell runs INSIDE the container (reports `Alpine Linux`, which a
    // non-Alpine host lacks), the bind-mounted workdir is visible, and the
    // container is removed at reap (`docker ps -a` shows no leftover `leviath-*`).

    /// The argv comes from `leviath_sys::container_*_argv`, which is never
    /// empty, but indexing `argv[0]` on trust is how a refactor becomes a panic.
    #[test]
    fn real_run_refuses_an_empty_argv_instead_of_indexing_it() {
        let err = real_run(&[]).unwrap_err();
        assert_eq!(err, "empty sandbox command");
    }
}
