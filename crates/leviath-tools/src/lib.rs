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
                name: "bash".to_string(),
                description: "Execute a shell command in the working directory. Use this for build commands, running tests, installing dependencies, or other shell operations. Has a 60-second timeout.".to_string(),
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
        ]
    }

    /// Names of all built-in tools.
    pub fn names(&self) -> Vec<String> {
        vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
            "list_dir".to_string(),
            "bash".to_string(),
        ]
    }

    /// Execute a built-in tool by name, returning the result as a string.
    pub async fn execute(&self, name: &str, args: Value) -> String {
        match name {
            "read_file" => self.read_file(&args).await,
            "write_file" => self.write_file(&args).await,
            "edit_file" => self.edit_file(&args).await,
            "list_dir" => self.list_dir(&args).await,
            "bash" => self.bash(&args).await,
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
                return format!("[error] Failed to create directories for '{}': {}", path_str, e);
            }
        }

        match std::fs::write(&path, content) {
            Ok(()) => format!("Successfully wrote {} bytes to '{}'", content.len(), path_str),
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
            0 => format!("[error] String not found in '{}'. Ensure old_str matches the file exactly.", path_str),
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
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

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

    async fn bash(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "[error] missing 'command' argument".to_string(),
        };

        let workdir = self.ctx.workdir.clone();

        let run = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&workdir)
            .output();

        match timeout(Duration::from_secs(60), run).await {
            Err(_) => format!("[timed out] Command exceeded 60s: {}", command),
            Ok(Err(e)) => format!("[error] Failed to spawn command: {}", e),
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
