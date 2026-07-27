//! Native built-in tools for Leviath agents.
//!
//! Provides file system and shell tools sandboxed to a working directory.

use leviath_providers::Tool;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

/// A platform feature a built-in tool depends on.
///
/// Each built-in declares the capabilities it requires (see
/// [`tool_required_capabilities`]); the current platform declares what it
/// provides (see [`PlatformCapabilities`]). A tool whose requirements aren't
/// met by the platform simply doesn't register — it's dropped from
/// advertisement, name-recognition, and dispatch. This lets platform-specific
/// tools (like `shell`, which spawns OS processes) coexist without per-tool
/// `#[cfg]` gates or one-off boolean flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCapability {
    /// Can launch child processes (e.g. the `shell` tool, stdio MCP servers).
    ProcessSpawn,
    /// Can read and write files.
    FileSystem,
    /// Can make outbound network/HTTP requests.
    Network,
}

/// The set of [`ToolCapability`]s the current platform provides.
///
/// Detection is compile-time: [`current`](Self::current) resolves to the
/// capability set for the build target. Desktop targets provide everything; a
/// future mobile or wasm host would provide a reduced set (no
/// [`ProcessSpawn`](ToolCapability::ProcessSpawn)). This mirrors the
/// `leviath-sys` crate's approach of centralizing platform `#[cfg]` branching
/// in one place rather than scattering it across call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCapabilities {
    capabilities: HashSet<ToolCapability>,
}

impl PlatformCapabilities {
    /// Capabilities for a desktop host: process spawning, filesystem, network.
    pub fn desktop() -> Self {
        Self {
            capabilities: HashSet::from([
                ToolCapability::ProcessSpawn,
                ToolCapability::FileSystem,
                ToolCapability::Network,
            ]),
        }
    }

    /// Capabilities for a mobile host: filesystem and network, but no process
    /// spawning (so the `shell` tool doesn't register).
    pub fn mobile() -> Self {
        Self {
            capabilities: HashSet::from([ToolCapability::FileSystem, ToolCapability::Network]),
        }
    }

    /// The capability set for the platform this binary was built for.
    ///
    /// Only desktop targets are built today, so this resolves to
    /// [`desktop`](Self::desktop). A future mobile/wasm target adds a `cfg` arm
    /// here returning [`mobile`](Self::mobile) — every built-in then auto-filters
    /// against it with no other change.
    pub fn current() -> Self {
        Self::desktop()
    }

    /// Build a capability set from an explicit collection (tests, future hosts).
    pub fn from_capabilities(caps: impl IntoIterator<Item = ToolCapability>) -> Self {
        Self {
            capabilities: caps.into_iter().collect(),
        }
    }

    /// Whether the platform provides `cap`.
    pub fn supports(&self, cap: ToolCapability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Whether the platform provides *every* capability in `required`. An empty
    /// requirement slice is always satisfied.
    pub fn satisfies(&self, required: &[ToolCapability]) -> bool {
        required.iter().all(|c| self.supports(*c))
    }
}

impl Default for PlatformCapabilities {
    fn default() -> Self {
        Self::current()
    }
}

/// The capabilities a built-in tool requires to function.
///
/// Keyed by *canonical* tool name (resolve aliases via [`canonical_tool_name`]
/// first). An empty slice means the tool is platform-agnostic and always
/// available — this covers the runtime-handled tools (`present_for_review`,
/// `ask_user_*`, `edit_document`, `context_*`) which touch neither the OS
/// process table nor the filesystem directly.
pub fn tool_required_capabilities(canonical_name: &str) -> &'static [ToolCapability] {
    match canonical_name {
        "shell" => &[ToolCapability::ProcessSpawn],
        "read_file" | "read_files" | "write_file" | "edit_file" | "list_dir" => {
            &[ToolCapability::FileSystem]
        }
        _ => &[],
    }
}

/// Context for tool execution — defines the sandbox root.
pub struct ToolContext {
    /// Absolute working directory. All file operations are confined here.
    pub workdir: PathBuf,
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
            file_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get (or create) the advisory lock for `path`. The map mutex is held only
    /// briefly to look up / insert; the returned per-file lock is what callers
    /// `.await` on across their read-modify-write.
    fn lock_for(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
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
/// executed. This is the single source of truth for aliases — [`names`],
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
/// Returns the input unchanged when it is not an alias — which includes every
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
/// The default (no executor) runs the command directly on the host — the exact
/// prior behavior. An implementor (the daemon's `SandboxManager`) returns a
/// [`tokio::process::Command`] that runs `command` inside a container or Linux
/// namespace instead. The implementor owns any per-stage sandbox state, so the
/// same handle is used for the agent's whole life; only shell execution is
/// affected (file tools stay on the host, over the bind-mounted workdir).
pub trait ShellExecutor: Send + Sync {
    /// Build the process that runs `command` via `shell flag` for `workdir`.
    fn build_command(&self, shell: &str, flag: &str, command: &str, workdir: &Path) -> Command;
}

/// Built-in tools: read_file, write_file, edit_file, list_dir, shell.
///
/// Carries the [`PlatformCapabilities`] of the current platform; tools whose
/// [`tool_required_capabilities`] aren't satisfied are dropped from
/// [`tool_defs`](Self::tool_defs), [`names`](Self::names), and rejected by
/// [`execute`](Self::execute).
pub struct BuiltinTools {
    ctx: ToolContext,
    platform: PlatformCapabilities,
    /// When set, shell commands run through this sandbox instead of the host.
    shell_executor: Option<Arc<dyn ShellExecutor>>,
}

impl BuiltinTools {
    /// Create a new BuiltinTools instance with the given sandbox context,
    /// filtering tools against the current platform's capabilities.
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            platform: PlatformCapabilities::current(),
            shell_executor: None,
        }
    }

    /// Route this agent's shell execution through `executor` (a container /
    /// namespace sandbox) instead of the host.
    pub fn with_shell_executor(mut self, executor: Arc<dyn ShellExecutor>) -> Self {
        self.shell_executor = Some(executor);
        self
    }

    /// Create a BuiltinTools instance with an explicit platform capability set,
    /// for tests or hosts that need to override the compile-time default.
    pub fn with_capabilities(ctx: ToolContext, platform: PlatformCapabilities) -> Self {
        Self {
            ctx,
            platform,
            shell_executor: None,
        }
    }

    /// Whether a built-in named `canonical_name` is available on this platform.
    fn available(&self, canonical_name: &str) -> bool {
        self.platform
            .satisfies(tool_required_capabilities(canonical_name))
    }

    /// All tool definitions to advertise to the LLM, minus any whose required
    /// platform capabilities aren't provided by the current platform.
    pub fn tool_defs(&self) -> Vec<Tool> {
        let mut defs = vec![
            Tool {
                name: "read_file".to_string(),
                description: "Read the complete contents of a file. Use this to examine existing code, configurations, or data files before making changes.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the working directory"
                        }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "write_file".to_string(),
                description: "Write content to a file, creating it (and any parent directories) if necessary. Use this to create new files or completely replace existing file content.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the working directory"
                        },
                        "content": {
                            "type": "string",
                            "description": "The full content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
            Tool {
                name: "edit_file".to_string(),
                description: "Replace an exact string in an existing file. The old_str must appear exactly once in the file. Use this for targeted edits rather than rewriting entire files.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the working directory"
                        },
                        "old_str": {
                            "type": "string",
                            "description": "The exact string to replace. Must appear exactly once in the file."
                        },
                        "new_str": {
                            "type": "string",
                            "description": "The string to replace old_str with"
                        }
                    },
                    "required": ["path", "old_str", "new_str"]
                }),
            },
            Tool {
                name: "list_dir".to_string(),
                description: "List the contents of a directory. Use this to explore the file structure before reading or writing files.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the directory, relative to the working directory. Defaults to the working directory root if omitted."
                        }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "read_files".to_string(),
                description: "Read multiple files at once. Returns the contents of all requested files in a single response, separated by file path headers. More efficient than calling read_file repeatedly. Use this when you need to read several files (e.g. after list_dir).".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Array of file paths relative to the working directory"
                        }
                    },
                    "required": ["paths"]
                }),
            },
            Tool {
                name: "shell".to_string(),
                description: "Execute a shell command in the working directory. Uses the system shell (bash/zsh on Unix, cmd on Windows). Use this for build commands, running tests, installing dependencies, or other shell operations. Has a 60-second timeout.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        }
                    },
                    "required": ["command"]
                }),
            },
            Tool {
                name: "present_for_review".to_string(),
                description: "Present a document, plan, or report to the user for review. The agent run will pause and the dashboard will display the document prominently. Use this when you want the user to read and approve something before you continue — for example, a technical design, an implementation plan, or a summary report. The user can provide feedback or simply acknowledge to continue.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Short title for the review prompt shown to the user (e.g. 'Implementation Plan Ready for Review')"
                        },
                        "markdown": {
                            "type": "string",
                            "description": "The markdown document to present to the user. Supports headings, lists, code blocks, and mermaid diagrams."
                        }
                    },
                    "required": ["title", "markdown"]
                }),
            },
            Tool {
                name: "ask_user_text".to_string(),
                description: "Ask the user a free-form question and wait for their written answer. The run pauses until they respond. Use this when you need clarification, missing information, or a specific detail only the user knows — decide for yourself when this is necessary; don't ask about things you can figure out on your own.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The question to ask the user"
                        }
                    },
                    "required": ["prompt"]
                }),
            },
            Tool {
                name: "ask_user_choice".to_string(),
                description: "Ask the user to pick one option from a list and wait for their answer. The run pauses until they respond. Use this when you have a small number of distinct paths forward and want the user to decide which one, rather than guessing yourself.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The question to ask the user"
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "At least two options for the user to choose from"
                        }
                    },
                    "required": ["prompt", "options"]
                }),
            },
            Tool {
                name: "ask_user_confirm".to_string(),
                description: "Ask the user a yes/no question and wait for their answer. The run pauses until they respond. Use this for a quick go/no-go decision before doing something significant or hard to undo.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The yes/no question to ask the user"
                        }
                    },
                    "required": ["prompt"]
                }),
            },
            Tool {
                name: "edit_document".to_string(),
                description: "Present a document to the user in an editable field pre-filled with its current text, and wait for them to edit it directly. The run pauses until they submit. Use this when the user wants to modify content themselves (e.g. tweak a plan or draft) rather than describe changes for you to make. Pass the current full text as `content`; the returned text is the user's edited version, which you should adopt as authoritative.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The current full document text to present for editing"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Optional instruction shown above the editable field"
                        }
                    },
                    "required": ["content"]
                }),
            },
            Tool {
                name: "context_write".to_string(),
                description: "Store or update content in a named section of your context window. This content will be included in your system prompt on subsequent turns, making it available for reference. Use this to save analysis, plans, notes, or structured information. If a key is provided and an entry with that key already exists, it will be replaced with the new content.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section (e.g. 'architecture', 'plan')"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key for the entry. Replaces existing entry with the same key."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to store"
                        }
                    },
                    "required": ["region", "content"]
                }),
            },
            Tool {
                name: "context_append".to_string(),
                description: "Add content to an existing section of your context window without replacing what's already there.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key for the entry"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to append"
                        }
                    },
                    "required": ["region", "content"]
                }),
            },
            Tool {
                name: "context_read".to_string(),
                description: "Read what's currently stored in a section of your context window. If no key is specified and the section contains keyed entries, returns a summary of all keys and their sizes.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section to read"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key of a specific entry to read"
                        }
                    },
                    "required": ["region"]
                }),
            },
            Tool {
                name: "context_delete".to_string(),
                description: "Remove a specific keyed entry from a section of your context window.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Name of the context window section"
                        },
                        "key": {
                            "type": "string",
                            "description": "Key of the entry to remove"
                        }
                    },
                    "required": ["region", "key"]
                }),
            },
            Tool {
                name: "context_list".to_string(),
                description: "List available sections of your context window with their current usage — section names, token counts, and number of entries. Use this to see what's available and what you've already stored.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "region": {
                            "type": "string",
                            "description": "Optional region name to list keys for"
                        }
                    },
                    "required": []
                }),
            },
        ];
        defs.retain(|t| self.available(&t.name));
        defs
    }

    /// Tool definitions for sub-agent management tools.
    ///
    /// These are advertised to the LLM but executed externally (by the CLI's
    /// tool registry) since they require access to the AgentEngine.
    pub fn subagent_tool_defs() -> Vec<Tool> {
        vec![
            Tool {
                name: "spawn_agent".to_string(),
                description: "Spawn a sub-agent from a blueprint to work on a task. Returns the new agent's ID. If wait=true, blocks until the sub-agent completes and returns its result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "blueprint": {
                            "type": "string",
                            "description": "Name of the agent blueprint to spawn"
                        },
                        "task": {
                            "type": "string",
                            "description": "Task prompt for the sub-agent"
                        },
                        "wait": {
                            "type": "boolean",
                            "description": "If true, block until the sub-agent completes and return its result. Default: false",
                            "default": false
                        },
                        "seed_context": {
                            "type": "string",
                            "description": "Optional initial context to inject into the sub-agent's first Pinned region"
                        },
                        "max_child_depth": {
                            "type": "integer",
                            "description": "Optional max depth for the sub-agent's own children"
                        }
                    },
                    "required": ["blueprint", "task"]
                }),
            },
            Tool {
                name: "check_agent".to_string(),
                description: "Check the status of a sub-agent. Returns its current status and result if complete. Non-blocking.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to check"
                        }
                    },
                    "required": ["agent_id"]
                }),
            },
            Tool {
                name: "wait_for_agent".to_string(),
                description: "Block until a sub-agent completes, then return its final result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to wait for"
                        }
                    },
                    "required": ["agent_id"]
                }),
            },
            Tool {
                name: "send_to_agent".to_string(),
                description: "Send a message to a running sub-agent's context window.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the target agent"
                        },
                        "message": {
                            "type": "string",
                            "description": "Message content to send"
                        },
                        "target_region": {
                            "type": "string",
                            "description": "Context region to deliver to (default: conversation)"
                        }
                    },
                    "required": ["agent_id", "message"]
                }),
            },
            Tool {
                name: "kill_agent".to_string(),
                description: "Kill a sub-agent and all its descendants. Sets their cancellation tokens and marks them as cancelled.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "ID of the agent to kill"
                        }
                    },
                    "required": ["agent_id"]
                }),
            },
        ]
    }

    /// Names of sub-agent tools.
    pub fn subagent_tool_names() -> Vec<String> {
        vec![
            "spawn_agent".to_string(),
            "check_agent".to_string(),
            "wait_for_agent".to_string(),
            "send_to_agent".to_string(),
            "kill_agent".to_string(),
        ]
    }

    /// Names of all built-in tools, including every alias in [`TOOL_ALIASES`].
    ///
    /// Aliases are included so tool-call dispatch recognizes a call arriving
    /// under an alias name as a built-in; the canonical names are what get
    /// advertised to the model.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = [
            "read_file",
            "read_files",
            "write_file",
            "edit_file",
            "list_dir",
            "shell",
            "present_for_review",
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
            "edit_document",
            "context_write",
            "context_append",
            "context_read",
            "context_delete",
            "context_list",
        ]
        .iter()
        // Drop any canonical built-in the current platform can't provide, so a
        // filtered-out tool (e.g. `shell` without `ProcessSpawn`) isn't even
        // recognized as a built-in on dispatch.
        .filter(|n| self.available(n))
        .map(|s| s.to_string())
        .collect();
        // Include an alias only when its canonical target survived filtering
        // (so `bash` disappears together with `shell`).
        names.extend(
            TOOL_ALIASES
                .iter()
                .filter(|(_, canonical)| self.available(canonical))
                .map(|(alias, _)| alias.to_string()),
        );
        names
    }

    /// Execute a built-in tool by name (resolving aliases), returning the result
    /// as a string.
    pub async fn execute(&self, name: &str, args: Value) -> String {
        let canonical = canonical_tool_name(name);
        // A tool whose platform capabilities aren't met never advertises, but a
        // caller could still dispatch to it directly — reject it here too.
        if !self.available(canonical) {
            return format!("[error] tool '{}' is not available on this platform", name);
        }
        match canonical {
            "read_file" => self.read_file(&args).await,
            "read_files" => self.read_files(&args).await,
            "write_file" => self.write_file(&args).await,
            "edit_file" => self.edit_file(&args).await,
            "list_dir" => self.list_dir(&args).await,
            "shell" => self.shell(&args).await,
            n if n.starts_with("context_") => {
                "[error] context tools must be handled by the runtime".to_string()
            }
            _ => format!("[error] Unknown built-in tool: {}", name),
        }
    }

    /// Refuse to create anything when the working directory itself is gone.
    ///
    /// `write_file` calls `create_dir_all`, which would otherwise silently
    /// resurrect a workspace an external harness deleted mid-run — leaving the
    /// agent writing into an empty tree that no longer resembles the checkout it
    /// reasoned about, and masking the loss from the runtime's health check
    /// (issue #107). Creating *sub*directories inside a live workspace is
    /// untouched; only a missing workspace root is refused.
    fn ensure_workspace(&self) -> Result<(), String> {
        if std::fs::metadata(&self.ctx.workdir).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        Err(format!(
            "[error] workspace '{}' is no longer accessible",
            self.ctx.workdir.display()
        ))
    }

    /// Resolve a requested path to an absolute path inside the workdir.
    ///
    /// Rejects paths that would escape the working directory via `../` components.
    fn resolve(&self, requested: &str) -> anyhow::Result<PathBuf> {
        let raw = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.ctx.workdir.join(requested)
        };

        // Normalize by resolving .. and . without requiring the path to exist.
        let mut normalized = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::ParentDir => {
                    if !normalized.pop() {
                        anyhow::bail!("path '{}' escapes the working directory", requested);
                    }
                }
                c => normalized.push(c),
            }
        }

        if !normalized.starts_with(&self.ctx.workdir) {
            anyhow::bail!("path '{}' would escape the working directory", requested);
        }

        Ok(normalized)
    }

    async fn read_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => format!("[error] Failed to read '{}': {}", path_str, e),
        }
    }

    async fn read_files(&self, args: &Value) -> String {
        let paths = match args.get("paths").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return "[error] missing 'paths' argument (expected array)".to_string(),
        };

        if paths.is_empty() {
            return "[error] 'paths' array is empty".to_string();
        }

        let mut results = Vec::with_capacity(paths.len());
        for path_val in paths {
            let path_str = match path_val.as_str() {
                Some(p) => p,
                None => {
                    results.push("[error] non-string path in array".to_string());
                    continue;
                }
            };

            let path = match self.resolve(path_str) {
                Ok(p) => p,
                Err(e) => {
                    results.push(format!("### [{}]\n[error] {}", path_str, e));
                    continue;
                }
            };

            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    results.push(format!("### [{}]\n{}", path_str, content));
                }
                Err(e) => {
                    results.push(format!("### [{}]\n[error] Failed to read: {}", path_str, e));
                }
            }
        }

        results.join("\n\n")
    }

    async fn write_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'content' argument".to_string(),
        };
        if let Err(e) = self.ensure_workspace() {
            return e;
        }

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        // Serialize concurrent writes to the same file (fan-out workers).
        let lock = self.ctx.lock_for(&path);
        let _guard = lock.lock().await;

        let parent = {
            let mut p = path.clone();
            p.pop();
            p
        };
        if let Err(e) = std::fs::create_dir_all(&parent) {
            return format!(
                "[error] Failed to create directories for '{}': {}",
                path_str, e
            );
        }

        match std::fs::write(&path, content) {
            Ok(()) => format!(
                "Successfully wrote {} bytes to '{}'",
                content.len(),
                path_str
            ),
            Err(e) => format!("[error] Failed to write '{}': {}", path_str, e),
        }
    }

    async fn edit_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };
        let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return "[error] missing 'old_str' argument".to_string(),
        };
        let new_str = match args.get("new_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return "[error] missing 'new_str' argument".to_string(),
        };
        if let Err(e) = self.ensure_workspace() {
            return e;
        }

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        // Serialize the read-modify-write against concurrent edits/writes to the
        // same file (fan-out workers), preventing lost updates.
        let lock = self.ctx.lock_for(&path);
        let _guard = lock.lock().await;

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("[error] Failed to read '{}': {}", path_str, e),
        };

        let count = content.matches(old_str).count();
        match count {
            0 => format!(
                "[error] String not found in '{}'. Ensure old_str matches the file exactly.",
                path_str
            ),
            1 => {
                let new_content = content.replacen(old_str, new_str, 1);
                match std::fs::write(&path, &new_content) {
                    Ok(()) => format!("Successfully edited '{}'", path_str),
                    Err(e) => format!("[error] Failed to write '{}': {}", path_str, e),
                }
            }
            n => format!(
                "[error] Found {} occurrences of the string in '{}'. old_str must be unique.",
                n, path_str
            ),
        }
    }

    async fn list_dir(&self, args: &Value) -> String {
        let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        let entries = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(e) => return format!("[error] Failed to read directory '{}': {}", path_str, e),
        };

        let mut items: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        items.sort_by_key(|e| e.file_name());

        let mut lines = Vec::new();
        for entry in items {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                lines.push(format!("{}/", name));
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                lines.push(format!("{} ({}B)", name, size));
            }
        }

        if lines.is_empty() {
            format!("(empty directory: {})", path_str)
        } else {
            lines.join("\n")
        }
    }

    /// Detect the best available shell on the system.
    ///
    /// Priority:
    /// - Windows: cmd.exe (always available)
    /// - Unix: $SHELL env var (user's preferred shell) → bash → zsh → sh
    fn detect_shell() -> (&'static str, &'static str) {
        #[cfg(windows)]
        {
            ("cmd.exe", "/C")
        }

        #[cfg(not(windows))]
        {
            Self::detect_shell_impl(std::env::var("SHELL").ok(), &|s| {
                std::path::Path::new(s).exists()
            })
        }
    }

    /// Core shell-detection logic with injectable env and filesystem checks
    /// for testing.
    ///
    /// `shell_exists` is a trait object (`&dyn Fn(&str) -> bool`) rather
    /// than `impl Fn(&str) -> bool` so every caller -- production's real
    /// `Path::exists` closure and each test's distinct closure -- shares
    /// exactly ONE monomorphization of this function instead of one per
    /// closure type (this function was a confirmed generic-monomorphization
    /// coverage-attribution artifact: every source position had a covered
    /// instantiation, but the summary table still reported some as missed).
    #[cfg(not(windows))]
    fn detect_shell_impl(
        env_shell: Option<String>,
        shell_exists: &dyn Fn(&str) -> bool,
    ) -> (&'static str, &'static str) {
        if let Some(shell) = env_shell
            && (shell.ends_with("/zsh") || shell.ends_with("/bash") || shell.ends_with("/sh"))
            && shell_exists(&shell)
        {
            // Only trust `$SHELL` when it actually exists — a stale or
            // sandbox-missing `$SHELL` (e.g. `/bin/zsh` in an environment that
            // doesn't ship it) otherwise made every shell call fail to spawn.
            // When it's missing, fall through to the known-path fallback list.
            let shell: &'static str = Box::leak(shell.into_boxed_str());
            return (shell, "-c");
        }
        for &shell in &[
            "/bin/bash",
            "/usr/bin/bash",
            "/bin/zsh",
            "/usr/bin/zsh",
            "/bin/sh",
        ] {
            if shell_exists(shell) {
                return (shell, "-c");
            }
        }
        ("sh", "-c")
    }

    async fn shell(&self, args: &Value) -> String {
        self.shell_with_timeout(args, Duration::from_secs(60)).await
    }

    /// Same as [`Self::shell`], with an injectable timeout so tests can
    /// exercise the timeout branch without a real 60-second wait.
    async fn shell_with_timeout(&self, args: &Value, timeout_duration: Duration) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'command' argument".to_string(),
        };

        let workdir = self.ctx.workdir.clone();
        let (shell, flag) = Self::detect_shell();

        // When a sandbox executor is attached, it builds a command that runs
        // inside a container / namespace (still targeting `workdir`); otherwise
        // run the shell directly on the host — the exact prior behavior.
        let mut cmd = match &self.shell_executor {
            Some(executor) => executor.build_command(shell, flag, command, &workdir),
            None => {
                let mut c = Command::new(shell);
                c.arg(flag).arg(command).current_dir(&workdir);
                c
            }
        };
        let run = cmd.output();

        match timeout(timeout_duration, run).await {
            Err(_) => format!("[timed out] Command exceeded 60s: {}", command),
            Ok(Err(e)) => format!("[error] Failed to spawn shell '{}': {}", shell, e),
            Ok(Ok(output)) => Self::format_command_output(
                &output.stdout,
                &output.stderr,
                output.status.success(),
                output.status.code().unwrap_or(-1),
            ),
        }
    }

    /// Format captured command output. Split out (behavior-preserving) from
    /// [`Self::shell_with_timeout`] so the success / non-zero-exit
    /// stdout+stderr formatting arms can be exercised deterministically on
    /// every platform, independent of the host shell's command-chaining and
    /// redirection syntax (`cmd.exe` and `sh` differ, so an integration test
    /// that produces stdout+stderr+non-zero-exit in one command is not
    /// portable).
    fn format_command_output(
        stdout: &[u8],
        stderr: &[u8],
        success: bool,
        exit_code: i32,
    ) -> String {
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);

        if success {
            if stdout.trim().is_empty() {
                "(command succeeded with no output)".to_string()
            } else {
                stdout.to_string()
            }
        } else {
            let mut result = format!("[exit code {}]\n", exit_code);
            if !stdout.trim().is_empty() {
                result.push_str(&format!("stdout:\n{}\n", stdout));
            }
            if !stderr.trim().is_empty() {
                result.push_str(&format!("stderr:\n{}", stderr));
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tools(dir: &std::path::Path) -> BuiltinTools {
        BuiltinTools::new(ToolContext::new(dir.to_path_buf()))
    }

    /// Built-ins over a mobile capability set (no `ProcessSpawn`), so the
    /// `shell` tool and its `bash` alias are filtered out.
    fn make_mobile_tools(dir: &std::path::Path) -> BuiltinTools {
        BuiltinTools::with_capabilities(
            ToolContext::new(dir.to_path_buf()),
            PlatformCapabilities::mobile(),
        )
    }

    // ── Tool definitions ──────────────────────────────────────────────────

    #[test]
    fn tool_defs_returns_sixteen_tools() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let defs = tools.tool_defs();
        assert_eq!(defs.len(), 16);
    }

    #[test]
    fn tool_defs_names_are_correct() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"read_files".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"edit_file".to_string()));
        assert!(names.contains(&"list_dir".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"present_for_review".to_string()));
        assert!(names.contains(&"ask_user_text".to_string()));
        assert!(names.contains(&"ask_user_choice".to_string()));
        assert!(names.contains(&"ask_user_confirm".to_string()));
        assert!(names.contains(&"edit_document".to_string()));
        assert!(names.contains(&"context_write".to_string()));
        assert!(names.contains(&"context_append".to_string()));
        assert!(names.contains(&"context_read".to_string()));
        assert!(names.contains(&"context_delete".to_string()));
        assert!(names.contains(&"context_list".to_string()));
    }

    #[test]
    fn tool_defs_edit_document_requires_content() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let def = tools
            .tool_defs()
            .into_iter()
            .find(|t| t.name == "edit_document")
            .expect("edit_document tool def must exist");
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "content"));
        assert_eq!(def.parameters["properties"]["content"]["type"], "string");
        // Also present in the builtin name list.
        assert!(tools.names().contains(&"edit_document".to_string()));
    }

    #[test]
    fn tool_defs_ask_user_choice_has_options_array() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let def = tools
            .tool_defs()
            .into_iter()
            .find(|t| t.name == "ask_user_choice")
            .unwrap();
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "prompt"));
        assert!(required.iter().any(|v| v == "options"));
        assert_eq!(def.parameters["properties"]["options"]["type"], "array");
    }

    #[tokio::test]
    async fn context_tools_return_runtime_error() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for name in [
            "context_write",
            "context_append",
            "context_read",
            "context_delete",
            "context_list",
        ] {
            let result = tools.execute(name, serde_json::json!({})).await;
            assert!(result.contains("context tools must be handled by the runtime"));
        }
    }

    #[tokio::test]
    async fn ask_user_tools_not_handled_by_builtin_execute() {
        // ask_user_* tools are intercepted upstream (worker.rs/foreground.rs),
        // exactly like present_for_review — execute() must never run them.
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for name in [
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
            "edit_document",
        ] {
            let result = tools.execute(name, serde_json::json!({})).await;
            assert!(result.contains("Unknown built-in tool"));
        }
    }

    #[test]
    fn context_tool_descriptions_mention_key_concepts() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let defs = tools.tool_defs();

        let write_def = defs.iter().find(|t| t.name == "context_write").unwrap();
        assert!(
            write_def.description.contains("system prompt"),
            "context_write should mention system prompt: {}",
            write_def.description
        );
        assert!(
            write_def.description.contains("replaced"),
            "context_write should mention replacement: {}",
            write_def.description
        );

        let read_def = defs.iter().find(|t| t.name == "context_read").unwrap();
        assert!(
            read_def.description.contains("summary"),
            "context_read should mention summary: {}",
            read_def.description
        );

        let list_def = defs.iter().find(|t| t.name == "context_list").unwrap();
        assert!(
            list_def.description.contains("token"),
            "context_list should mention tokens: {}",
            list_def.description
        );

        let append_def = defs.iter().find(|t| t.name == "context_append").unwrap();
        assert!(
            append_def.description.contains("without replacing"),
            "context_append should mention 'without replacing': {}",
            append_def.description
        );
    }

    fn assert_has_description(name: &str, description: &str) {
        assert!(
            !description.is_empty(),
            "tool {} has empty description",
            name
        );
    }

    fn assert_has_object_params(name: &str, params: &serde_json::Value) {
        assert!(params.is_object(), "tool {} has non-object params", name);
    }

    #[test]
    fn tool_defs_have_descriptions() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for def in tools.tool_defs() {
            assert_has_description(&def.name, &def.description);
        }
    }

    #[test]
    #[should_panic(expected = "tool bogus has empty description")]
    fn tool_defs_have_descriptions_panics_on_empty_description() {
        assert_has_description("bogus", "");
    }

    #[test]
    fn tool_defs_have_parameters() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for def in tools.tool_defs() {
            assert_has_object_params(&def.name, &def.parameters);
        }
    }

    #[test]
    #[should_panic(expected = "tool bogus has non-object params")]
    fn tool_defs_have_parameters_panics_on_non_object_params() {
        assert_has_object_params("bogus", &serde_json::Value::Null);
    }

    // ── names() ───────────────────────────────────────────────────────────

    #[test]
    fn names_includes_bash_alias() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names = tools.names();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"shell".to_string()));
    }

    #[test]
    fn canonical_tool_name_resolves_aliases_and_passes_others_through() {
        // An alias resolves to its canonical name.
        assert_eq!(canonical_tool_name("bash"), "shell");
        // A canonical built-in is unchanged.
        assert_eq!(canonical_tool_name("shell"), "shell");
        assert_eq!(canonical_tool_name("read_file"), "read_file");
        // An unknown name (e.g. an MCP tool whose server may not be installed)
        // passes through untouched, so it is matched/omitted as-is.
        assert_eq!(canonical_tool_name("acme__do_thing"), "acme__do_thing");
        // Every alias in the table round-trips to a real canonical name.
        for (alias, canonical) in TOOL_ALIASES {
            assert_eq!(canonical_tool_name(alias), *canonical);
        }
    }

    #[test]
    fn names_returns_seventeen_entries() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        assert_eq!(tools.names().len(), 17);
    }

    // ── Sub-agent tool definitions ────────────────────────────────────────

    #[test]
    fn subagent_tool_defs_returns_five_tools() {
        let defs = BuiltinTools::subagent_tool_defs();
        assert_eq!(defs.len(), 5);
    }

    #[test]
    fn subagent_tool_names_returns_five_names() {
        let names = BuiltinTools::subagent_tool_names();
        assert_eq!(names.len(), 5);
        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"check_agent".to_string()));
        assert!(names.contains(&"wait_for_agent".to_string()));
        assert!(names.contains(&"send_to_agent".to_string()));
        assert!(names.contains(&"kill_agent".to_string()));
    }

    #[test]
    fn subagent_tool_defs_names_match_subagent_tool_names() {
        let defs = BuiltinTools::subagent_tool_defs();
        let names = BuiltinTools::subagent_tool_names();
        let def_names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        assert_eq!(def_names, names);
    }

    // ── resolve() ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_relative_path() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.resolve("hello.txt").unwrap();
        assert!(result.starts_with(&tools.ctx.workdir));
        assert!(result.ends_with("hello.txt"));
    }

    #[test]
    fn resolve_rejects_path_escape() {
        let dir = std::env::temp_dir().join("leviath_test_sandbox");
        fs::create_dir_all(&dir).ok();
        let tools = make_tools(&dir);
        let result = tools.resolve("../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_dot_stays_in_workdir() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.resolve("./foo/./bar.txt").unwrap();
        assert!(result.starts_with(&tools.ctx.workdir));
        assert!(result.ends_with("foo/bar.txt"));
    }

    // ── execute() with file I/O (async) ───────────────────────────────────

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.execute("nonexistent", json!({})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Unknown built-in tool"));
    }

    #[tokio::test]
    async fn read_file_missing_path_arg() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let result = tools.execute("read_file", json!({})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("missing 'path'"));
    }

    #[tokio::test]
    async fn write_and_read_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let write_result = tools
            .execute(
                "write_file",
                json!({"path": "test.txt", "content": "hello world"}),
            )
            .await;
        assert!(write_result.contains("Successfully wrote"));
        assert!(write_result.contains("11 bytes"));

        let read_result = tools
            .execute("read_file", json!({"path": "test.txt"}))
            .await;
        assert_eq!(read_result, "hello world");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let result = tools
            .execute(
                "write_file",
                json!({"path": "sub/dir/file.txt", "content": "nested"}),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        assert!(dir.path().join("sub/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn write_tools_refuse_to_resurrect_a_deleted_workspace() {
        // Issue #107: an external harness deletes the workspace mid-run.
        // `create_dir_all` would happily recreate it and let the agent write
        // into an empty tree that no longer resembles the checkout it reasoned
        // about — and the runtime's health check, which just stats the workdir,
        // would never see it was gone.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("workspace");
        fs::create_dir(&workdir).unwrap();
        fs::write(workdir.join("a.txt"), "before").unwrap();
        let tools = make_tools(&workdir);
        fs::remove_dir_all(&workdir).unwrap();

        for (tool, args) in [
            ("write_file", json!({"path": "a.txt", "content": "after"})),
            (
                "edit_file",
                json!({"path": "a.txt", "old_str": "before", "new_str": "after"}),
            ),
        ] {
            let result = tools.execute(tool, args).await;
            assert!(
                result.contains("workspace") && result.contains("no longer accessible"),
                "{tool} got: {result}"
            );
        }
        assert!(!workdir.exists(), "the workspace must stay gone");
    }

    #[tokio::test]
    async fn write_file_missing_content_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("write_file", json!({"path": "f.txt"})).await;
        assert!(result.contains("missing 'content'"));
    }

    #[tokio::test]
    async fn write_file_missing_path_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("write_file", json!({"content": "x"})).await;
        assert!(result.contains("missing 'path'"));
    }

    #[test]
    fn resolve_rejects_excessive_parent_dir_traversal() {
        // A *relative, nonexistent* workdir keeps `resolve`'s accumulator free
        // of any platform-specific leading root/drive/prefix components:
        // `canonicalize` fails for a path that doesn't exist (on every OS), so
        // `ToolContext::new` keeps the raw relative `PathBuf` verbatim. The
        // request then decomposes into exactly `[Normal(workdir), ParentDir,
        // ParentDir, ...]`; the first `..` pops the single workdir component and
        // the second `..` calls `normalized.pop()` on an *empty* accumulator,
        // which returns `false` -- firing the "escapes the working directory"
        // bail deterministically on every OS.
        //
        // (An empty "" workdir is not portable here: on Windows `canonicalize("")`
        // can succeed and yield an absolute cwd whose Prefix/RootDir components
        // absorb the `..`, so `pop()` never fails and this bail is never hit --
        // which is exactly why this branch was Windows-uncovered before.)
        let tools = BuiltinTools::new(ToolContext::new(PathBuf::from(
            "leviath-nonexistent-relative-workdir",
        )));
        let result = tools.resolve("../../etc/passwd");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes the working directory")
        );
    }

    #[tokio::test]
    async fn edit_file_successful_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        tools
            .execute(
                "write_file",
                json!({"path": "e.txt", "content": "foo bar baz"}),
            )
            .await;

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "e.txt", "old_str": "bar", "new_str": "qux"}),
            )
            .await;
        assert!(result.contains("Successfully edited"));

        let content = tools.execute("read_file", json!({"path": "e.txt"})).await;
        assert_eq!(content, "foo qux baz");
    }

    #[tokio::test]
    async fn edit_file_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        tools
            .execute("write_file", json!({"path": "e.txt", "content": "abc"}))
            .await;

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "e.txt", "old_str": "xyz", "new_str": "123"}),
            )
            .await;
        assert!(result.contains("String not found"));
    }

    #[tokio::test]
    async fn edit_file_missing_file_returns_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "does-not-exist.txt", "old_str": "a", "new_str": "b"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read"));
    }

    #[tokio::test]
    async fn edit_file_multiple_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        tools
            .execute("write_file", json!({"path": "e.txt", "content": "aaa aaa"}))
            .await;

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "e.txt", "old_str": "aaa", "new_str": "bbb"}),
            )
            .await;
        assert!(result.contains("2 occurrences"));
        assert!(result.contains("must be unique"));
    }

    #[tokio::test]
    async fn edit_file_missing_args() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        let r1 = tools.execute("edit_file", json!({})).await;
        assert!(r1.contains("missing 'path'"));

        let r2 = tools.execute("edit_file", json!({"path": "f.txt"})).await;
        assert!(r2.contains("missing 'old_str'"));

        let r3 = tools
            .execute("edit_file", json!({"path": "f.txt", "old_str": "x"}))
            .await;
        assert!(r3.contains("missing 'new_str'"));
    }

    #[tokio::test]
    async fn list_dir_contents() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let result = tools.execute("list_dir", json!({})).await;
        assert!(result.contains("a.txt"));
        assert!(result.contains("subdir/"));
    }

    #[tokio::test]
    async fn list_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("list_dir", json!({})).await;
        assert!(result.contains("empty directory"));
    }

    #[tokio::test]
    async fn list_dir_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());

        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/inner.txt"), "data").unwrap();

        let result = tools.execute("list_dir", json!({"path": "sub"})).await;
        assert!(result.contains("inner.txt"));
    }

    #[tokio::test]
    async fn read_file_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("read_file", json!({"path": "nope.txt"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read"));
    }

    // ── read_files (batch reads) ────────────────────────────────────────────

    #[tokio::test]
    async fn read_files_multiple_valid_files() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        fs::write(dir.path().join("b.txt"), "beta").unwrap();

        let result = tools
            .execute("read_files", json!({"paths": ["a.txt", "b.txt"]}))
            .await;
        assert!(result.contains("### [a.txt]"));
        assert!(result.contains("alpha"));
        assert!(result.contains("### [b.txt]"));
        assert!(result.contains("beta"));
        // Results are joined with a blank line between entries.
        assert!(result.contains("\n\n"));
    }

    #[tokio::test]
    async fn read_files_missing_paths_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("read_files", json!({})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("missing 'paths'"));
    }

    #[tokio::test]
    async fn read_files_non_array_paths_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        // A string (not an array) → as_array() returns None → same error path.
        let result = tools.execute("read_files", json!({"paths": "a.txt"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("missing 'paths'"));
    }

    #[tokio::test]
    async fn read_files_empty_paths_array() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("read_files", json!({"paths": []})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("empty"));
    }

    #[tokio::test]
    async fn read_files_missing_file_reports_per_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("present.txt"), "here").unwrap();

        let result = tools
            .execute(
                "read_files",
                json!({"paths": ["present.txt", "absent.txt"]}),
            )
            .await;
        // Valid file still returned…
        assert!(result.contains("### [present.txt]"));
        assert!(result.contains("here"));
        // …while the missing one produces a per-file error under its header.
        assert!(result.contains("### [absent.txt]"));
        assert!(result.contains("Failed to read"));
    }

    #[tokio::test]
    async fn read_files_non_string_element_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("ok.txt"), "content").unwrap();

        let result = tools
            .execute("read_files", json!({"paths": ["ok.txt", 42]}))
            .await;
        assert!(result.contains("content"));
        assert!(result.contains("non-string path in array"));
    }

    #[tokio::test]
    async fn read_files_path_escape_reported_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("read_files", json!({"paths": ["../../etc/passwd"]}))
            .await;
        assert!(result.contains("### [../../etc/passwd]"));
        assert!(result.contains("escape"));
    }

    // ── resolve() absolute paths ────────────────────────────────────────────

    #[test]
    fn resolve_absolute_path_inside_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        // Build the absolute path from the tool's own (canonicalized) workdir
        // rather than `dir.path()` directly — on macOS `/tmp`/`/var` are
        // symlinks, so the two can differ even though they're the same place.
        let abs = tools.ctx.workdir.join("inside.txt");
        let result = tools.resolve(abs.to_str().unwrap()).unwrap();
        assert_eq!(result, abs);
    }

    #[test]
    fn resolve_rejects_absolute_path_outside_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.resolve("/etc/passwd");
        assert!(result.is_err());
    }

    // ── path-escape rejection propagates through each tool ─────────────────

    #[tokio::test]
    async fn read_file_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("read_file", json!({"path": "../../etc/passwd"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    #[tokio::test]
    async fn write_file_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute(
                "write_file",
                json!({"path": "../../evil.txt", "content": "x"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    #[tokio::test]
    async fn edit_file_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute(
                "edit_file",
                json!({"path": "../../evil.txt", "old_str": "a", "new_str": "b"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    #[tokio::test]
    async fn list_dir_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("list_dir", json!({"path": "../../"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("escape"));
    }

    // ── filesystem failure branches ─────────────────────────────────────────

    #[tokio::test]
    async fn write_file_fails_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::create_dir(dir.path().join("adir")).unwrap();

        let result = tools
            .execute("write_file", json!({"path": "adir", "content": "x"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to write"));
    }

    #[tokio::test]
    async fn write_file_parent_dir_creation_fails_when_blocked_by_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        // "blocker" exists as a plain file, so create_dir_all("blocker") must fail.
        fs::write(dir.path().join("blocker"), "im a file").unwrap();

        let result = tools
            .execute(
                "write_file",
                json!({"path": "blocker/nested.txt", "content": "x"}),
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to create directories"));
    }

    #[tokio::test]
    async fn read_file_fails_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::create_dir(dir.path().join("adir")).unwrap();

        let result = tools.execute("read_file", json!({"path": "adir"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read"));
    }

    #[tokio::test]
    async fn list_dir_fails_when_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        fs::write(dir.path().join("afile.txt"), "content").unwrap();

        let result = tools
            .execute("list_dir", json!({"path": "afile.txt"}))
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to read directory"));
    }

    // `set_readonly(false)` widens Unix perms beyond the original, but here it
    // only re-enables cleanup of a throwaway tempdir file, which is exactly
    // what we want.
    #[allow(clippy::permissions_set_readonly_false)]
    #[tokio::test]
    async fn edit_file_write_failure_after_successful_match() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let file_path = dir.path().join("ro.txt");
        fs::write(&file_path, "hello world").unwrap();

        // Make the file read-only so the read succeeds but the write-back
        // fails. `set_readonly(true)` is cross-platform (clears the write bits
        // on Unix; sets the read-only attribute on Windows), so the write
        // error arm is exercised on every OS.
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file_path, perms).unwrap();

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "ro.txt", "old_str": "hello", "new_str": "goodbye"}),
            )
            .await;

        // Restore permissions so tempdir cleanup can remove the file.
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_readonly(false);
        fs::set_permissions(&file_path, perms).unwrap();

        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to write"));
    }

    #[tokio::test]
    async fn shell_echo_command() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("shell", json!({"command": "echo hello"}))
            .await;
        assert!(result.trim().contains("hello"));
    }

    #[tokio::test]
    async fn bash_alias_works() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute("bash", json!({"command": "echo alias_test"}))
            .await;
        assert!(result.contains("alias_test"));
    }

    #[tokio::test]
    async fn shell_missing_command_arg() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("shell", json!({})).await;
        assert!(result.contains("missing 'command'"));
    }

    /// A `ShellExecutor` that ignores the requested command and instead runs a
    /// fixed marker command — proof that shell execution is routed through it.
    struct RedirectExecutor;
    impl ShellExecutor for RedirectExecutor {
        fn build_command(
            &self,
            shell: &str,
            flag: &str,
            _command: &str,
            workdir: &Path,
        ) -> Command {
            let mut c = Command::new(shell);
            c.arg(flag).arg("echo SANDBOXED").current_dir(workdir);
            c
        }
    }

    #[tokio::test]
    async fn shell_routes_through_executor_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let tools = BuiltinTools::new(ToolContext::new(dir.path().to_path_buf()))
            .with_shell_executor(Arc::new(RedirectExecutor));
        // The agent asked for `echo host`, but the executor redirects it.
        let result = tools
            .execute("shell", json!({"command": "echo host"}))
            .await;
        assert!(result.contains("SANDBOXED"), "got: {result}");
        assert!(!result.contains("host"));
    }

    #[tokio::test]
    async fn shell_failing_command() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("shell", json!({"command": "false"})).await;
        assert!(result.contains("[exit code"));
    }

    #[tokio::test]
    async fn shell_successful_command_with_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools.execute("shell", json!({"command": "true"})).await;
        assert_eq!(result, "(command succeeded with no output)");
    }

    // The stdout+stderr non-zero-exit formatting is asserted directly against
    // `format_command_output` (below) rather than via a real shell command:
    // producing stdout, stderr, and a non-zero exit in a single command needs
    // shell-specific syntax (`;`/`1>&2` on `sh`, `&`/redirection on `cmd.exe`)
    // that isn't portable, and this session already hit real Windows CI
    // failures from insufficiently-verified platform-specific test commands.
    #[test]
    fn format_command_output_non_zero_exit_reports_stdout_and_stderr() {
        let result = BuiltinTools::format_command_output(b"out-line\n", b"err-line\n", false, 1);
        assert!(result.contains("[exit code 1]"));
        assert!(result.contains("stdout:"));
        assert!(result.contains("out-line"));
        assert!(result.contains("stderr:"));
        assert!(result.contains("err-line"));
    }

    #[test]
    fn format_command_output_non_zero_exit_omits_empty_streams() {
        // Whitespace-only streams are treated as empty and neither the
        // stdout: nor stderr: block is emitted.
        let result = BuiltinTools::format_command_output(b"   \n", b"", false, 2);
        assert_eq!(result, "[exit code 2]\n");
    }

    #[test]
    fn format_command_output_success_with_output_returns_stdout() {
        let result = BuiltinTools::format_command_output(b"hello\n", b"", true, 0);
        assert_eq!(result, "hello\n");
    }

    #[test]
    fn format_command_output_success_no_output() {
        let result = BuiltinTools::format_command_output(b"   ", b"noise", true, 0);
        assert_eq!(result, "(command succeeded with no output)");
    }

    #[tokio::test]
    async fn shell_with_timeout_fires_on_slow_command() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .shell_with_timeout(&json!({"command": "sleep 5"}), Duration::from_millis(100))
            .await;
        assert!(result.contains("[timed out]"));
    }

    #[tokio::test]
    async fn shell_spawn_failure_when_workdir_missing() {
        // A workdir that doesn't exist on disk makes Command::output() fail
        // before the shell ever runs (current_dir() can't chdir into it).
        // canonicalize() fails for a nonexistent path, so ToolContext::new()
        // falls back to keeping the raw (nonexistent) path as-is.
        let tools = make_tools(std::path::Path::new(
            "/definitely/does/not/exist/leviath-test",
        ));
        let result = tools.execute("shell", json!({"command": "echo hi"})).await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Failed to spawn shell"));
    }

    // ── ToolContext ────────────────────────────────────────────────────────

    #[test]
    fn tool_context_new_canonicalizes() {
        let dir = std::env::temp_dir();
        let ctx = ToolContext::new(dir.clone());
        // Canonicalized path should be absolute
        assert!(ctx.workdir.is_absolute());
    }

    #[test]
    fn tool_context_new_with_nonexistent_dir() {
        let ctx = ToolContext::new(PathBuf::from("/nonexistent/path/unlikely"));
        // Falls back to the original path when canonicalization fails
        assert_eq!(ctx.workdir, PathBuf::from("/nonexistent/path/unlikely"));
    }

    // ── detect_shell ──────────────────────────────────────────────────────

    /// Windows' `detect_shell()` branch is a plain, unconditional constant
    /// return (no env/filesystem dependence to inject) -- the
    /// platform-agnostic `detect_shell_returns_valid_shell` test below
    /// already exercises it on Windows CI, but this asserts the exact
    /// documented return value directly.
    #[cfg(windows)]
    #[test]
    fn detect_shell_returns_cmd_exe() {
        let (shell, flag) = BuiltinTools::detect_shell();
        assert_eq!(shell, "cmd.exe");
        assert_eq!(flag, "/C");
    }

    #[test]
    fn detect_shell_returns_valid_shell() {
        // Pure reader: `detect_shell()` always returns a non-empty shell (and the
        // "-c" flag on non-Windows) regardless of $SHELL, so it is robust to a
        // concurrent temp-env writer and needs no serialization of its own.
        let (shell, flag) = BuiltinTools::detect_shell();
        assert!(!shell.is_empty());
        assert!(!flag.is_empty());
        #[cfg(not(windows))]
        assert_eq!(flag, "-c");
    }

    /// Forces `detect_shell()` to exercise the real `shell_exists` closure by
    /// temporarily setting $SHELL to an unrecognized path, causing the candidate
    /// loop (and the closure) to be reached. `temp_env::with_var` sets the var,
    /// runs the closure, and restores it -- serialized against every other
    /// temp-env test process-wide, so no hand-rolled lock is needed.
    #[cfg(not(windows))]
    #[test]
    fn detect_shell_queries_real_filesystem_for_unrecognized_shell() {
        let (shell, flag) =
            temp_env::with_var("SHELL", Some("/opt/not-a-recognized-shell"), || {
                BuiltinTools::detect_shell()
            });
        assert_eq!(flag, "-c");
        assert!(
            [
                "/bin/bash",
                "/usr/bin/bash",
                "/bin/zsh",
                "/usr/bin/zsh",
                "/bin/sh"
            ]
            .contains(&shell)
        );
    }

    // ── detect_shell_impl() — inject env and filesystem for full branch coverage ──

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_zsh_from_env() {
        // `$SHELL` is trusted only when it exists on disk.
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/local/bin/zsh".to_string()), &|s| {
                s == "/usr/local/bin/zsh"
            });
        assert_eq!(shell, "/usr/local/bin/zsh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_bash_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/local/bin/bash".to_string()), &|s| {
                s == "/usr/local/bin/bash"
            });
        assert_eq!(shell, "/usr/local/bin/bash");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_sh_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/bin/sh".to_string()), &|s| {
                s == "/usr/bin/sh"
            });
        assert_eq!(shell, "/usr/bin/sh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_falls_back_when_env_shell_is_missing() {
        // Regression for #79: `$SHELL` is a recognized shell name but does not
        // exist on disk (a stale or sandbox-missing `/bin/zsh`). It must NOT be
        // returned — fall through to an available fallback instead of failing
        // every shell call with "No such file or directory".
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/bin/zsh".to_string()), &|s| s == "/bin/sh");
        assert_eq!(shell, "/bin/sh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_falls_through_when_env_unrecognized() {
        // /opt/fish doesn't end with /zsh, /bash, or /sh → falls to candidate loop
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/opt/fish".to_string()), &|s| s == "/bin/bash");
        assert_eq!(shell, "/bin/bash");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_skips_missing_candidates_and_finds_zsh() {
        // bash paths return false; /bin/zsh exists — covers shell_exists false branch
        let (shell, flag) = BuiltinTools::detect_shell_impl(None, &|s| s == "/bin/zsh");
        assert_eq!(shell, "/bin/zsh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_last_resort_when_nothing_exists() {
        let (shell, flag) = BuiltinTools::detect_shell_impl(None, &|_| false);
        assert_eq!(shell, "sh");
        assert_eq!(flag, "-c");
    }

    #[tokio::test]
    async fn concurrent_edits_same_file_serialize_no_lost_update() {
        // Two workers edit different unique strings in the SAME file at once.
        // The per-path lock serializes the read-modify-write, so both edits
        // land; without it, the second write would clobber the first.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "A\nB\n").unwrap();
        let tools = std::sync::Arc::new(make_tools(dir.path()));

        let t1 = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute(
                    "edit_file",
                    json!({"path": "f.txt", "old_str": "A", "new_str": "A1"}),
                )
                .await
            })
        };
        let t2 = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute(
                    "edit_file",
                    json!({"path": "f.txt", "old_str": "B", "new_str": "B2"}),
                )
                .await
            })
        };
        let (r1, r2) = tokio::join!(t1, t2);
        assert!(!r1.unwrap().starts_with("[error]"));
        assert!(!r2.unwrap().starts_with("[error]"));

        let final_content = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(
            final_content, "A1\nB2\n",
            "both concurrent edits must apply (no lost update)"
        );
    }

    #[tokio::test]
    async fn concurrent_writes_different_files_both_succeed() {
        // Different files never contend on the per-path lock.
        let dir = tempfile::tempdir().unwrap();
        let tools = std::sync::Arc::new(make_tools(dir.path()));

        let a = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute("write_file", json!({"path": "a.txt", "content": "AAA"}))
                    .await
            })
        };
        let b = {
            let t = tools.clone();
            tokio::spawn(async move {
                t.execute("write_file", json!({"path": "b.txt", "content": "BBB"}))
                    .await
            })
        };
        let (ra, rb) = tokio::join!(a, b);
        assert!(!ra.unwrap().starts_with("[error]"));
        assert!(!rb.unwrap().starts_with("[error]"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "AAA"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "BBB"
        );
    }

    // ── Platform capabilities ─────────────────────────────────────────────

    #[test]
    fn desktop_supports_all_capabilities() {
        let caps = PlatformCapabilities::desktop();
        assert!(caps.supports(ToolCapability::ProcessSpawn));
        assert!(caps.supports(ToolCapability::FileSystem));
        assert!(caps.supports(ToolCapability::Network));
    }

    #[test]
    fn mobile_lacks_process_spawn() {
        let caps = PlatformCapabilities::mobile();
        assert!(!caps.supports(ToolCapability::ProcessSpawn));
        assert!(caps.supports(ToolCapability::FileSystem));
        assert!(caps.supports(ToolCapability::Network));
    }

    #[test]
    fn current_matches_desktop_and_is_the_default() {
        // Only desktop targets are built today.
        assert_eq!(
            PlatformCapabilities::current(),
            PlatformCapabilities::desktop()
        );
        assert_eq!(
            PlatformCapabilities::default(),
            PlatformCapabilities::desktop()
        );
    }

    #[test]
    fn satisfies_requires_all_and_empty_is_always_met() {
        let caps = PlatformCapabilities::mobile();
        assert!(caps.satisfies(&[]));
        assert!(caps.satisfies(&[ToolCapability::FileSystem]));
        assert!(!caps.satisfies(&[ToolCapability::ProcessSpawn]));
        // All-or-nothing: one unmet requirement fails the whole set.
        assert!(!caps.satisfies(&[ToolCapability::FileSystem, ToolCapability::ProcessSpawn]));
    }

    #[test]
    fn from_capabilities_builds_explicit_set() {
        let caps = PlatformCapabilities::from_capabilities([ToolCapability::Network]);
        assert!(caps.supports(ToolCapability::Network));
        assert!(!caps.supports(ToolCapability::FileSystem));
    }

    #[test]
    fn tool_required_capabilities_by_name() {
        assert_eq!(
            tool_required_capabilities("shell"),
            &[ToolCapability::ProcessSpawn]
        );
        assert_eq!(
            tool_required_capabilities("read_file"),
            &[ToolCapability::FileSystem]
        );
        // Runtime-handled / platform-agnostic tools require nothing.
        assert!(tool_required_capabilities("context_write").is_empty());
        assert!(tool_required_capabilities("present_for_review").is_empty());
        assert!(tool_required_capabilities("unknown_tool").is_empty());
    }

    #[test]
    fn mobile_tool_defs_omit_shell_but_keep_the_rest() {
        let dir = std::env::temp_dir();
        let tools = make_mobile_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(!names.contains(&"shell".to_string()));
        // The other 15 built-ins remain.
        assert_eq!(tools.tool_defs().len(), 15);
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"context_write".to_string()));
        assert!(names.contains(&"present_for_review".to_string()));
    }

    #[test]
    fn desktop_tool_defs_include_shell() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"shell".to_string()));
    }

    #[test]
    fn mobile_names_omit_shell_and_bash_alias() {
        let dir = std::env::temp_dir();
        let tools = make_mobile_tools(&dir);
        let names = tools.names();
        assert!(!names.contains(&"shell".to_string()));
        assert!(!names.contains(&"bash".to_string()));
        // File + context tools still recognized.
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"context_write".to_string()));
    }

    #[test]
    fn desktop_names_include_shell_and_bash_alias() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names = tools.names();
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"bash".to_string()));
    }

    #[tokio::test]
    async fn mobile_execute_shell_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_mobile_tools(dir.path());
        let out = tools.execute("shell", json!({"command": "echo hi"})).await;
        assert!(out.contains("not available on this platform"), "got: {out}");
        // The `bash` alias resolves to `shell` and is rejected the same way.
        let out = tools.execute("bash", json!({"command": "echo hi"})).await;
        assert!(out.contains("not available on this platform"), "got: {out}");
    }

    #[tokio::test]
    async fn mobile_execute_file_tool_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_mobile_tools(dir.path());
        let out = tools
            .execute("write_file", json!({"path": "x.txt", "content": "hi"}))
            .await;
        assert!(!out.starts_with("[error]"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.txt")).unwrap(),
            "hi"
        );
    }
}
