//! The sandbox context, tool-name aliases, and the shell-executor seam.

use super::*;

/// Context for tool execution - defines the sandbox root.
pub struct ToolContext {
    /// Absolute working directory. All file operations are confined here.
    pub workdir: PathBuf,
    /// The `[read_paths]` policy: which paths outside the workdir the
    /// *read-only* file tools may fall back to, and only when both the
    /// blueprint declares them and the user's config grants them. Inactive by
    /// default, and never consulted by `write_file`/`edit_file` - writes are
    /// confined to `workdir` unconditionally.
    ///
    /// Behind a lock because a run re-reads its grants when it resumes: a
    /// person who answers a refused read by granting the path in `config.toml`
    /// has to be able to resume the run rather than start it again. Read once
    /// per resolve, so the lock is never held across any I/O.
    read_paths: Mutex<leviath_core::ReadPathPolicy>,
    /// Per-path advisory locks serializing concurrent mutating file operations
    /// (`write_file`/`edit_file`) on the *same* file. Fan-out sub-agent workers
    /// share one process and one workdir, so an in-process lock map keyed by
    /// canonical path is sufficient (no OS `flock` needed) to prevent lost
    /// updates when two workers touch the same file. Different files never
    /// contend.
    file_locks: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
    /// Which of the daemon's environment variables a shell command inherits.
    /// Resolved from `[security]` at spawn; the default withholds
    /// credential-shaped names.
    pub(crate) shell_env: ShellEnvPolicy,
    /// Where `install_tool` writes: the global tools directory every agent
    /// scans at spawn, [`leviath_core::tools_dir`] by default. `None` when no
    /// home directory resolves, in which case `install_tool` refuses. Not the
    /// workdir and not under the workdir fence: this is the one built-in that
    /// legitimately writes outside the run.
    pub tools_dir: Option<PathBuf>,
    /// Names `install_tool` refuses on top of this platform's built-ins and the
    /// sub-agent tools: the MCP tools the run offers, filled at spawn. A script
    /// under any of these is dropped at discovery, so installing one would
    /// report a tool that never runs.
    pub reserved_names: Vec<String>,
}

/// The resolved `[security] shell_env` decision for one run.
///
/// Carried as data rather than consulted from config at each call, so the
/// executor has no opinion about where the decision came from and the same
/// struct serves the shell tool, a Rhai `shell()`, and a command seed.
#[derive(Debug, Clone, Default)]
pub struct ShellEnvPolicy {
    /// Which of the four filtering modes is in effect.
    pub mode: leviath_core::ShellEnvMode,
    /// Names handed over under every mode, from `[security] allow_env_vars`.
    /// The same list a Rhai `env_var` read goes through, so there is one answer
    /// to "may agent-supplied code see this variable".
    pub allow_env_vars: Vec<String>,
    /// Names withheld under `custom`, where the built-in name-shape heuristic
    /// is off and only the explicit lists govern. Ignored in the other modes.
    pub withhold: Vec<String>,
}

impl ShellEnvPolicy {
    /// Strip the variables this policy withholds from `cmd`.
    ///
    /// Applied to a built `Command` rather than to an environment map, so one
    /// call covers however the caller decided to run the thing: the host shell,
    /// a namespace sandbox (which isolates mounts and network but still
    /// inherits the environment), and the fallback that runs on the host when
    /// namespaces turn out to be unusable. A container exec inherits nothing,
    /// so this is a no-op there.
    pub fn apply(&self, cmd: &mut tokio::process::Command) -> Vec<String> {
        // `inherit` is the "behave as before" escape hatch, so it should cost
        // what it did before: nothing. Without this it still walks and
        // allocates the whole environment to decide it wants none of it.
        if self.mode == leviath_core::ShellEnvMode::Inherit {
            return Vec::new();
        }
        let names: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .collect();
        let withheld = leviath_core::withheld_child_vars(
            names.iter().map(String::as_str),
            self.mode,
            &self.allow_env_vars,
            &self.withhold,
        );
        for name in &withheld {
            cmd.env_remove(name);
        }
        withheld
    }
}

impl ToolContext {
    /// Create a new context. Attempts to canonicalize the working directory.
    pub fn new(workdir: PathBuf) -> Self {
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        Self {
            workdir,
            read_paths: Mutex::new(leviath_core::ReadPathPolicy::inactive()),
            file_locks: Arc::new(Mutex::new(HashMap::new())),
            shell_env: ShellEnvPolicy::default(),
            tools_dir: leviath_core::tools_dir(),
            reserved_names: Vec::new(),
        }
    }

    /// Point `install_tool` at a directory other than the data root's
    /// `tools/`, or at nothing at all. Builder-style. Tests use it to install
    /// into a temporary directory and to reach the no-home refusal without
    /// touching the environment.
    pub fn with_tools_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.tools_dir = dir;
        self
    }

    /// The names `install_tool` refuses beyond the built-ins it can see for
    /// itself: at spawn, the run's MCP tool names. Builder-style.
    pub fn with_reserved_names(mut self, names: Vec<String>) -> Self {
        self.reserved_names = names;
        self
    }

    /// Attach a `[read_paths]` policy resolved at spawn. Builder-style, like
    /// [`BuiltinTools::with_shell_executor`].
    pub fn with_read_paths(self, policy: leviath_core::ReadPathPolicy) -> Self {
        self.set_read_paths(policy);
        self
    }

    /// Replace the `[read_paths]` policy on a context already in service.
    ///
    /// What a resume calls: the grants are resolved from `config.toml`, which
    /// the daemon re-reads, and a run parked on a refused read is exactly the
    /// case where the person has just gone and granted the path.
    pub fn set_read_paths(&self, policy: leviath_core::ReadPathPolicy) {
        *self
            .read_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = policy;
    }

    /// The `[read_paths]` policy in force right now.
    pub(crate) fn read_paths(&self) -> leviath_core::ReadPathPolicy {
        self.read_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Attach the `[security] shell_env` decision resolved at spawn.
    pub fn with_shell_env(mut self, policy: ShellEnvPolicy) -> Self {
        self.shell_env = policy;
        self
    }

    /// Get (or create) the advisory lock for `path`. The map mutex is held only
    /// briefly to look up / insert; the returned per-file lock is what callers
    /// `.await` on across their read-modify-write.
    pub(crate) fn lock_for(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .file_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// Alias → canonical built-in tool name.
///
/// A blueprint's `available_tools` may name a built-in by any alias listed here;
/// it resolves to the canonical tool that is advertised to the model and
/// executed. This is the single source of truth for aliases - [`names`],
/// [`BuiltinTools::execute`], and the daemon's `available_tools` filtering all go
/// through [`canonical_tool_name`], so adding a row here is all it takes to add
/// an alias everywhere. Add rows only for genuine synonyms of an existing tool.
///
/// [`names`]: BuiltinTools::names
pub const TOOL_ALIASES: &[(&str, &str)] = &[
    // `bash` is the familiar name for the general shell tool.
    ("bash", "shell"),
];

/// Resolve `name` through [`TOOL_ALIASES`] to its canonical built-in name.
///
/// Returns the input unchanged when it is not an alias - which includes every
/// canonical built-in and every MCP tool name, so this is safe to apply to any
/// tool name before matching it against a definition.
pub fn canonical_tool_name(name: &str) -> &str {
    for (alias, canonical) in TOOL_ALIASES {
        if *alias == name {
            return canonical;
        }
    }
    name
}

/// Every name that refers to the same tool as `name`: the name itself, its
/// canonical form, and every alias of that canonical form.
///
/// For matching a tool against something a *person* wrote, rather than against
/// a tool definition. [`canonical_tool_name`] is enough when the written name is
/// the one being resolved, but not when it is the key of a map being searched: a
/// call the model makes is always canonical (`shell`), so looking up only
/// `shell` and its canonical form never finds a `bash` entry, however many
/// spellings the writer had to choose from.
///
/// The first item is always `name`, so a caller that stops at the first hit
/// prefers the exact spelling.
pub fn tool_name_spellings(name: &str) -> impl Iterator<Item = &str> {
    let canonical = canonical_tool_name(name);
    std::iter::once(name)
        .chain(std::iter::once(canonical))
        .chain(
            TOOL_ALIASES
                .iter()
                .filter(move |(_, c)| *c == canonical)
                .map(|(alias, _)| *alias),
        )
        .filter({
            let mut seen: Vec<&str> = Vec::new();
            move |s| match seen.contains(s) {
                true => false,
                false => {
                    seen.push(s);
                    true
                }
            }
        })
}

/// Redirects shell command execution off the host into a sandbox.
///
/// The default (no executor) runs the command directly on the host. An
/// implementor (the daemon's `SandboxManager`) returns a
/// [`tokio::process::Command`] that runs `command` inside a container or Linux
/// namespace instead. The implementor owns any per-stage sandbox state, so the
/// same handle is used for the agent's whole life; only shell execution is
/// affected (file tools stay on the host, over the bind-mounted workdir).
pub trait ShellExecutor: Send + Sync {
    /// Build the process that runs `command` via `shell flag` for `workdir`.
    fn build_command(&self, shell: &str, flag: &str, command: &str, workdir: &Path) -> Command;
}

#[cfg(test)]
mod shell_env_tests {
    use super::*;

    /// Asserts on the *built* command rather than a spawned one: `get_envs`
    /// reports an explicit removal as `(name, None)`, which is deterministic on
    /// every platform and needs no child process.
    fn removed(policy: &ShellEnvPolicy) -> Vec<String> {
        let mut cmd = Command::new("sh");
        policy.apply(&mut cmd);
        cmd.as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .filter_map(|(k, _)| k.to_str().map(str::to_string))
            .collect()
    }

    /// The seam works end to end: a credential-shaped variable present in the
    /// daemon's own environment is removed from the child, and `PATH` - which
    /// every real command needs - is not.
    #[test]
    fn the_default_policy_strips_a_credential_but_not_the_path() {
        temp_env::with_vars(
            [
                ("LEV_TEST_FAKE_API_KEY", Some("secret")),
                ("LEV_TEST_ORDINARY", Some("fine")),
            ],
            || {
                let out = removed(&ShellEnvPolicy::default());
                assert!(out.iter().any(|n| n == "LEV_TEST_FAKE_API_KEY"));
                assert!(!out.iter().any(|n| n == "LEV_TEST_ORDINARY"));
                assert!(!out.iter().any(|n| n == "PATH"));
            },
        );
    }

    /// The builder carries the decision through to where the executor reads it.
    /// Without this the policy resolves at spawn and is then dropped on the
    /// floor, which every other assertion here would still pass.
    #[test]
    fn the_builder_carries_the_policy_to_the_context() {
        let ctx = ToolContext::new(std::env::temp_dir()).with_shell_env(ShellEnvPolicy {
            mode: leviath_core::ShellEnvMode::Custom,
            withhold: vec!["LEV_TEST_NAMED".to_string()],
            ..Default::default()
        });
        assert_eq!(ctx.shell_env.mode, leviath_core::ShellEnvMode::Custom);
        temp_env::with_var("LEV_TEST_NAMED", Some("x"), || {
            assert_eq!(removed(&ctx.shell_env), ["LEV_TEST_NAMED"]);
        });
    }

    /// `inherit` is the escape hatch, and it must actually touch nothing.
    #[test]
    fn inherit_removes_nothing() {
        temp_env::with_var("LEV_TEST_FAKE_API_KEY", Some("secret"), || {
            let policy = ShellEnvPolicy {
                mode: leviath_core::ShellEnvMode::Inherit,
                ..Default::default()
            };
            assert!(removed(&policy).is_empty());
        });
    }
}
