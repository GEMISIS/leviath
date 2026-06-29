//! Native built-in tools for Leviath agents.
//!
//! Provides file system and shell tools sandboxed to a working directory.

use leviath_providers::Tool;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Context for tool execution — defines the sandbox root.
pub struct ToolContext {
    /// Absolute working directory. All file operations are confined here.
    pub workdir: PathBuf,
}

impl ToolContext {
    /// Create a new context. Attempts to canonicalize the working directory.
    pub fn new(workdir: PathBuf) -> Self {
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        Self { workdir }
    }
}

/// Built-in tools: read_file, write_file, edit_file, list_dir, bash.
pub struct BuiltinTools {
    ctx: ToolContext,
}

impl BuiltinTools {
    /// Create a new BuiltinTools instance with the given sandbox context.
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }

    /// All tool definitions to advertise to the LLM.
    pub fn tool_defs(&self) -> Vec<Tool> {
        vec![
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
        ]
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

    /// Names of all built-in tools.
    pub fn names(&self) -> Vec<String> {
        vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
            "list_dir".to_string(),
            "shell".to_string(),
            "bash".to_string(), // Alias for backward compatibility
            "present_for_review".to_string(),
        ]
    }

    /// Execute a built-in tool by name, returning the result as a string.
    pub async fn execute(&self, name: &str, args: Value) -> String {
        match name {
            "read_file" => self.read_file(&args).await,
            "write_file" => self.write_file(&args).await,
            "edit_file" => self.edit_file(&args).await,
            "list_dir" => self.list_dir(&args).await,
            "shell" | "bash" => self.shell(&args).await,
            _ => format!("[error] Unknown built-in tool: {}", name),
        }
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
                Component::CurDir => {}
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

    async fn write_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "[error] missing 'path' argument".to_string(),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'content' argument".to_string(),
        };

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!(
                    "[error] Failed to create directories for '{}': {}",
                    path_str, e
                );
            }
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

        let path = match self.resolve(path_str) {
            Ok(p) => p,
            Err(e) => return format!("[error] {}", e),
        };

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
            // Check user's preferred shell first
            if let Ok(shell) = std::env::var("SHELL") {
                if shell.ends_with("/zsh") || shell.ends_with("/bash") || shell.ends_with("/sh") {
                    // Leak the string so we can return a static ref — this only runs once per detect
                    // and the shell path lives for the process lifetime anyway
                    let shell: &'static str = Box::leak(shell.into_boxed_str());
                    return (shell, "-c");
                }
            }

            // Fallback: try common shells in order
            for shell in &[
                "/bin/bash",
                "/usr/bin/bash",
                "/bin/zsh",
                "/usr/bin/zsh",
                "/bin/sh",
            ] {
                if std::path::Path::new(shell).exists() {
                    return (shell, "-c");
                }
            }

            // Last resort
            ("sh", "-c")
        }
    }

    async fn shell(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'command' argument".to_string(),
        };

        let workdir = self.ctx.workdir.clone();
        let (shell, flag) = Self::detect_shell();

        let run = Command::new(shell)
            .arg(flag)
            .arg(command)
            .current_dir(&workdir)
            .output();

        match timeout(Duration::from_secs(60), run).await {
            Err(_) => format!("[timed out] Command exceeded 60s: {}", command),
            Ok(Err(e)) => format!("[error] Failed to spawn shell '{}': {}", shell, e),
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                if output.status.success() {
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
    }
}
