//! The sandbox context, tool-name aliases, and the shell-executor seam.

use super::*;

/// Context for tool execution - defines the sandbox root.
pub struct ToolContext {
    /// Absolute working directory. All file operations are confined here.
    pub workdir: PathBuf,
    /// Additional directories/patterns this agent may read from beyond its
    /// workdir. C-suite agents use this to read design docs and run archives.
    /// Read-only; write operations are always sandboxed to `workdir`.
    pub read_paths: Vec<leviath_core::ReadPathEntry>,
    /// Per-path advisory locks serializing concurrent mutating file operations
    /// (`write_file`/`edit_file`) on the *same* file. Fan-out sub-agent workers
    /// share one process and one workdir, so an in-process lock map keyed by
    /// canonical path is sufficient (no OS `flock` needed) to prevent lost
    /// updates when two workers touch the same file. Different files never
    /// contend.
    file_locks: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ToolContext {
    /// Create a new context. Attempts to canonicalize the working directory.
    pub fn new(workdir: PathBuf) -> Self {
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        Self {
            workdir,
            read_paths: Vec::new(),
            file_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new context with additional read paths for the allowlist.
    pub fn with_read_paths(workdir: PathBuf, read_paths: Vec<leviath_core::ReadPathEntry>) -> Self {
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        Self {
            workdir,
            read_paths,
            file_locks: Arc::new(Mutex::new(HashMap::new())),
        }
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

/// Redirects shell command execution off the host into a sandbox.
///
/// The default (no executor) runs the command directly on the host - the exact
/// prior behavior. An implementor (the daemon's `SandboxManager`) returns a
/// [`tokio::process::Command`] that runs `command` inside a container or Linux
/// namespace instead. The implementor owns any per-stage sandbox state, so the
/// same handle is used for the agent's whole life; only shell execution is
/// affected (file tools stay on the host, over the bind-mounted workdir).
pub trait ShellExecutor: Send + Sync {
    /// Build the process that runs `command` via `shell flag` for `workdir`.
    fn build_command(&self, shell: &str, flag: &str, command: &str, workdir: &Path) -> Command;
}
