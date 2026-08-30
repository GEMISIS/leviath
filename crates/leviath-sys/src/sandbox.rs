//! Sandbox command construction for isolated tool execution.
//!
//! Builds the argv needed to run a shell command inside a container or a fresh
//! set of Linux namespaces (`unshare(1)`), plus container-engine detection. This
//! module is **pure argv assembly + PATH probing** - it never spawns a process
//! itself. The caller (the daemon's `SandboxManager`) owns the actual
//! `tokio::process::Command` spawn and container lifecycle, so everything here is
//! deterministic and unit-testable without any runtime installed.
//!
//! The container engine is just a **binary name** (`"docker"`, `"podman"`,
//! `"nerdctl"`, `"finch"`, …): Leviath isn't prescriptive about which one you
//! run, only that it speaks the common `run`/`exec`/`rm` verbs. Auto-detection
//! ([`detect_container_engine`]) prefers Docker then Podman, but a blueprint can
//! name any binary.
//!
//! Keeping the sole namespace-vs-container / Linux-vs-not knowledge here (behind
//! [`namespace_supported`] and a `cfg!`) means callers stay platform-agnostic,
//! consistent with the rest of this crate.

/// The container engines auto-detection probes for, in preference order. A
/// blueprint may name any other Docker-CLI-compatible binary explicitly.
pub const KNOWN_ENGINES: &[&str] = &["docker", "podman"];

/// Detect an available container engine on `PATH`, returning its binary name.
/// Prefers Docker, then Podman (see [`KNOWN_ENGINES`]).
pub fn detect_container_engine() -> Option<String> {
    detect_container_engine_with(&binary_on_path)
}

/// Testable core of [`detect_container_engine`]: `exists` reports whether a
/// binary name is available. First match in [`KNOWN_ENGINES`] wins.
pub(crate) fn detect_container_engine_with(exists: &dyn Fn(&str) -> bool) -> Option<String> {
    KNOWN_ENGINES
        .iter()
        .find(|bin| exists(bin))
        .map(|bin| bin.to_string())
}

/// Whether `bin` resolves to a regular file on any `PATH` entry.
pub(crate) fn binary_on_path(bin: &str) -> bool {
    binary_on_path_in(std::env::var_os("PATH"), bin)
}

/// Testable core of [`binary_on_path`]: `path` is the raw `PATH` value (`None`
/// when the variable is unset, which yields `false`).
fn binary_on_path_in(path: Option<std::ffi::OsString>, bin: &str) -> bool {
    match path {
        Some(paths) => std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()),
        None => false,
    }
}

/// Whether Linux-namespace sandboxing (`unshare`) is possible on this host.
/// Only Linux ships the namespaces + `unshare(1)` this relies on.
pub fn namespace_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Parameters for creating a long-lived, exec-able container.
#[derive(Debug, Clone)]
pub struct ContainerRunSpec<'a> {
    /// The engine binary to invoke (e.g. `"docker"`, `"podman"`, `"nerdctl"`).
    pub engine: &'a str,
    /// Container image, e.g. `"ubuntu:24.04"`.
    pub image: &'a str,
    /// Absolute host workdir; bind-mounted at the same path and set as the
    /// container's working directory.
    pub workdir: &'a str,
    /// Whether the container has network access (`false` → `--network none`).
    pub network: bool,
    /// Extra host paths to bind-mount (each mounted at its own path).
    pub mounts: &'a [String],
    /// Container name, used later for `exec` and `rm`.
    pub name: &'a str,
}

/// Host paths that must never be bind-mounted into an agent's container.
///
/// `mounts` comes from the blueprint - a file the user downloaded - and reaches
/// `-v {m}:{m}`. `mounts = ["/var/run/docker.sock"]`
/// is a one-line container escape (the container can then create a *privileged*
/// container on the host), and `mounts = ["/"]` makes the isolation decorative.
///
/// Matched as path prefixes, so `/var/run/docker.sock/..`-style near-misses and
/// anything under a forbidden root are refused together.
const FORBIDDEN_MOUNTS: &[&str] = &[
    "/",
    "/proc",
    "/sys",
    "/dev",
    "/etc",
    "/boot",
    "/var/run",
    "/run",
    // The container runtime's own socket, under either common path.
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/var/run/podman",
];

/// Whether `path` is safe to bind-mount into an agent container.
///
/// Refuses a fixed set of host-infrastructure roots and anything beneath them
/// (`/`, `/proc`, `/sys`, `/dev`, `/etc`, `/boot`, `/run`, `/var/run`, and the
/// container engines' own sockets), plus any
/// relative path (which the engine would resolve against its own cwd, not ours)
/// and anything containing `..`.
pub(crate) fn mount_allowed(path: &str) -> bool {
    // POSIX semantics spelled out rather than `std::path::Path`, because these
    // are paths *inside a Linux container* - the host's rules do not apply to
    // them. On Windows `Path::new("/data").is_absolute()` is false (an absolute
    // path there needs a drive letter), so routing this through `Path` would
    // refuse every legitimate container mount on Windows while behaving
    // correctly on Unix.
    let absolute = path.starts_with('/');
    let traverses = path.split('/').any(|segment| segment == "..");
    if !absolute || traverses {
        return false;
    }
    // Normalize a trailing slash so "/etc/" and "/etc" compare alike.
    let normalized = path.trim_end_matches('/');
    let normalized = match normalized.is_empty() {
        true => "/",
        false => normalized,
    };
    !FORBIDDEN_MOUNTS.iter().any(|forbidden| {
        normalized == *forbidden
            || (*forbidden != "/" && normalized.starts_with(&format!("{forbidden}/")))
            || *forbidden == "/" && normalized == "/"
    })
}

/// argv to start a detached, auto-removed container that idles (`sleep
/// infinity`) so shell calls can `exec` into it repeatedly (warm container).
///
/// Hardened beyond plain `run`: the container drops every capability, cannot
/// regain privileges via setuid binaries, and is bounded in processes and
/// memory. Without them the container runs as **root inside** with the default
/// capability set, so "sandboxed" buys isolation of the filesystem view and
/// nothing else, and anything written to the bind-mounted workdir comes back
/// root-owned on the host.
///
/// `mounts` entries are filtered through `mount_allowed`; a refused entry is
/// dropped rather than silently honored.
pub fn container_run_argv(spec: &ContainerRunSpec) -> Vec<String> {
    let mut v = vec![
        spec.engine.to_string(),
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        spec.name.to_string(),
        // No capabilities: an agent's shell needs none of them, and `CAP_SYS_ADMIN`
        // or `CAP_DAC_OVERRIDE` in particular turn a container into a foothold.
        "--cap-drop".to_string(),
        "ALL".to_string(),
        // A setuid binary inside the image cannot escalate back to root.
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        // Bound fork bombs and runaway memory so one agent cannot take the host
        // down with it.
        "--pids-limit".to_string(),
        "512".to_string(),
        "--memory".to_string(),
        "2g".to_string(),
    ];
    if !spec.network {
        v.push("--network".to_string());
        v.push("none".to_string());
    }
    // The agent's workdir is always mounted at the same path so file tools
    // (which run on the host) and shell tools (which run in the container) see
    // identical paths.
    v.push("-v".to_string());
    v.push(format!("{0}:{0}", spec.workdir));
    for m in spec.mounts.iter().filter(|m| mount_allowed(m)) {
        v.push("-v".to_string());
        v.push(format!("{m}:{m}"));
    }
    v.push("-w".to_string());
    v.push(spec.workdir.to_string());
    v.push(spec.image.to_string());
    v.push("sleep".to_string());
    v.push("infinity".to_string());
    v
}

/// argv to run one shell command inside a running container.
///
/// `shell`/`flag` are the shell *inside the container* (typically `sh`/`-c`,
/// which every image ships) - NOT the host's shell, whose absolute path may not
/// exist in the image.
pub fn container_exec_argv(
    engine: &str,
    name: &str,
    workdir: &str,
    shell: &str,
    flag: &str,
    command: &str,
) -> Vec<String> {
    vec![
        engine.to_string(),
        "exec".to_string(),
        "-w".to_string(),
        workdir.to_string(),
        name.to_string(),
        shell.to_string(),
        flag.to_string(),
        command.to_string(),
    ]
}

/// argv to force-remove a container (best-effort teardown).
pub fn container_rm_argv(engine: &str, name: &str) -> Vec<String> {
    vec![
        engine.to_string(),
        "rm".to_string(),
        "-f".to_string(),
        name.to_string(),
    ]
}

/// argv to run one shell command under fresh Linux namespaces via `unshare(1)`.
///
/// Uses an unprivileged user namespace (`--user --map-root-user`) so it works
/// without root, plus fresh mount + PID namespaces (`--mount --pid --fork
/// --mount-proc`). `network = false` adds `--net`, giving an empty network
/// namespace with no connectivity. The caller sets the child's working
/// directory, so no `cd` is embedded here.
///
/// # This is not a filesystem sandbox
///
/// There is no `pivot_root`, no `chroot`, and no seccomp filter: the command
/// **shares the host root filesystem** and can read `~/.ssh` or write
/// `~/.leviath/providers/*.rhai` exactly as an unsandboxed command could. What
/// `namespace` actually buys is PID isolation and, with `network = false`, no
/// connectivity - genuinely useful, and not what most people mean by "sandbox".
///
/// Choose `kind = "container"` when the goal is to contain what an agent can
/// *reach*. This mode is for bounding what it can *see running* and talk to.
/// Said plainly here because a caller who assumes otherwise gets a weaker
/// guarantee than they think, and the name invites that assumption.
pub fn namespace_argv(shell: &str, flag: &str, command: &str, network: bool) -> Vec<String> {
    let mut v = vec![
        "unshare".to_string(),
        "--user".to_string(),
        "--map-root-user".to_string(),
        "--mount".to_string(),
        "--pid".to_string(),
        "--fork".to_string(),
        "--mount-proc".to_string(),
    ];
    if !network {
        v.push("--net".to_string());
    }
    v.push(shell.to_string());
    v.push(flag.to_string());
    v.push(command.to_string());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_prefers_docker_then_podman_then_none() {
        assert_eq!(
            // `.contains` (not `||`) so no short-circuited operand region is left
            // uncovered when "docker" matches first.
            detect_container_engine_with(&|b| ["docker", "podman"].contains(&b)).as_deref(),
            Some("docker")
        );
        assert_eq!(
            detect_container_engine_with(&|b| b == "podman").as_deref(),
            Some("podman")
        );
        assert_eq!(detect_container_engine_with(&|_| false), None);
    }

    #[test]
    fn real_detection_and_path_probe_do_not_panic() {
        // Result varies by host; calling exercises the real PATH-reading wrapper.
        let _ = detect_container_engine();
        assert!(!binary_on_path("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn binary_on_path_in_handles_present_and_absent_path() {
        // Absent PATH → never found.
        assert!(!binary_on_path_in(None, "sh"));
        // A PATH containing a dir with a known file resolves it. Build a PATH
        // from a temp dir holding a marker file (portable across OSes).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "x").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(binary_on_path_in(Some(path.clone()), "marker"));
        assert!(!binary_on_path_in(Some(path), "not-there"));
    }

    #[test]
    fn namespace_supported_matches_target() {
        assert_eq!(namespace_supported(), cfg!(target_os = "linux"));
    }

    /// `mounts` comes from a downloaded blueprint. Mounting the engine's own
    /// socket lets the container create a privileged container on the host -
    /// a one-line escape - and mounting `/` makes the isolation decorative.
    #[test]
    fn forbidden_mounts_are_refused() {
        for path in [
            "/",
            "/proc",
            "/sys",
            "/dev",
            "/etc",
            "/etc/shadow",
            "/var/run",
            "/var/run/docker.sock",
            "/run/docker.sock",
            "/var/run/podman/podman.sock",
        ] {
            assert!(!mount_allowed(path), "{path} must be refused");
        }
    }

    /// Relative paths resolve against the *engine's* cwd, not ours, so they are
    /// never what the author meant; `..` is traversal.
    #[test]
    fn relative_and_traversing_mounts_are_refused() {
        for path in ["data", "./data", "/work/../etc", "../etc"] {
            assert!(!mount_allowed(path), "{path} must be refused");
        }
    }

    /// Ordinary project directories still mount - the denylist is about host
    /// infrastructure, not about making the feature unusable.
    ///
    /// The rule is POSIX, not host-native: a container path is a Linux path
    /// wherever the daemon happens to be running, so `/data` is absolute here
    /// even on Windows, where `std::path::Path` says otherwise.
    #[test]
    fn mount_rules_do_not_depend_on_the_host_platform() {
        // Absolute in POSIX terms, on every host.
        assert!(mount_allowed("/data"));
        assert!(mount_allowed("/home/u/project"));
        // A Windows-style path is not a container path, and is refused.
        assert!(!mount_allowed(r"C:\data"));
        assert!(!mount_allowed(r"\\server\share"));
        // Traversal is caught by segment, not by host path parsing.
        assert!(!mount_allowed("/data/../etc"));
        assert!(!mount_allowed("/.."));
        assert!(!mount_allowed(".."));
    }

    #[test]
    fn ordinary_mounts_are_allowed() {
        for path in [
            "/data",
            "/home/user/project",
            "/Users/me/code",
            "/opt/cache",
            // Not `/etc` itself, and not under it.
            "/etcetera",
        ] {
            assert!(mount_allowed(path), "{path} should be allowed");
        }
    }

    /// A refused mount is dropped from the argv rather than passed through.
    #[test]
    fn container_run_argv_drops_forbidden_mounts() {
        let spec = ContainerRunSpec {
            engine: "docker",
            image: "ubuntu:24.04",
            workdir: "/work",
            network: true,
            mounts: &["/var/run/docker.sock".to_string(), "/data".to_string()],
            name: "lev-abc",
        };
        let argv = container_run_argv(&spec);
        assert!(
            !argv.iter().any(|a| a.contains("docker.sock")),
            "the engine socket must never reach the argv: {argv:?}"
        );
        assert!(argv.iter().any(|a| a == "/data:/data"));
    }

    #[test]
    fn container_run_argv_with_network() {
        let spec = ContainerRunSpec {
            engine: "docker",
            image: "ubuntu:24.04",
            workdir: "/work",
            network: true,
            mounts: &["/data".to_string()],
            name: "lev-abc",
        };
        assert_eq!(
            container_run_argv(&spec),
            vec![
                "docker",
                "run",
                "-d",
                "--rm",
                "--name",
                "lev-abc",
                // Hardening flags: no capabilities, no privilege regain, and
                // bounded processes/memory. Without them the container runs
                // as root inside with the default capability set.
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--pids-limit",
                "512",
                "--memory",
                "2g",
                "-v",
                "/work:/work",
                "-v",
                "/data:/data",
                "-w",
                "/work",
                "ubuntu:24.04",
                "sleep",
                "infinity"
            ]
        );
    }

    #[test]
    fn container_run_argv_no_network_uses_none() {
        let spec = ContainerRunSpec {
            engine: "podman",
            image: "node:22-slim",
            workdir: "/w",
            network: false,
            mounts: &[],
            name: "lev-x",
        };
        let argv = container_run_argv(&spec);
        assert_eq!(argv[0], "podman");
        assert!(
            argv.windows(2)
                .any(|w| w == ["--network".to_string(), "none".to_string()])
        );
    }

    #[test]
    fn container_run_argv_accepts_arbitrary_engine() {
        // Non-prescriptive: any Docker-CLI-compatible binary works.
        let spec = ContainerRunSpec {
            engine: "nerdctl",
            image: "alpine",
            workdir: "/w",
            network: true,
            mounts: &[],
            name: "lev-n",
        };
        assert_eq!(container_run_argv(&spec)[0], "nerdctl");
    }

    #[test]
    fn container_exec_argv_shape() {
        assert_eq!(
            container_exec_argv("docker", "lev-abc", "/work", "sh", "-c", "ls -la"),
            vec![
                "docker", "exec", "-w", "/work", "lev-abc", "sh", "-c", "ls -la"
            ]
        );
    }

    #[test]
    fn container_rm_argv_shape() {
        assert_eq!(
            container_rm_argv("podman", "lev-abc"),
            vec!["podman", "rm", "-f", "lev-abc"]
        );
    }

    #[test]
    fn namespace_argv_isolated_network() {
        let argv = namespace_argv("sh", "-c", "whoami", false);
        assert_eq!(argv[0], "unshare");
        assert!(argv.contains(&"--net".to_string()));
        assert_eq!(&argv[argv.len() - 3..], &["sh", "-c", "whoami"]);
    }

    #[test]
    fn namespace_argv_shared_network_omits_net() {
        let argv = namespace_argv("bash", "-c", "echo hi", true);
        assert!(!argv.contains(&"--net".to_string()));
        assert!(argv.contains(&"--user".to_string()));
    }
}
