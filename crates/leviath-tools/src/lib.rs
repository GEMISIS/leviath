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
            "ask_user_text".to_string(),
            "ask_user_choice".to_string(),
            "ask_user_confirm".to_string(),
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
            Self::detect_shell_impl(std::env::var("SHELL").ok(), |s| {
                std::path::Path::new(s).exists()
            })
        }
    }

    /// Core shell-detection logic with injectable env and filesystem checks for testing.
    #[cfg(not(windows))]
    fn detect_shell_impl(
        env_shell: Option<String>,
        shell_exists: impl Fn(&str) -> bool,
    ) -> (&'static str, &'static str) {
        if let Some(shell) = env_shell {
            if shell.ends_with("/zsh") || shell.ends_with("/bash") || shell.ends_with("/sh") {
                let shell: &'static str = Box::leak(shell.into_boxed_str());
                return (shell, "-c");
            }
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

        let run = Command::new(shell)
            .arg(flag)
            .arg(command)
            .current_dir(&workdir)
            .output();

        match timeout(timeout_duration, run).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tools(dir: &std::path::Path) -> BuiltinTools {
        BuiltinTools::new(ToolContext::new(dir.to_path_buf()))
    }

    // ── Tool definitions ──────────────────────────────────────────────────

    #[test]
    fn tool_defs_returns_nine_tools() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let defs = tools.tool_defs();
        assert_eq!(defs.len(), 9);
    }

    #[test]
    fn tool_defs_names_are_correct() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        let names: Vec<String> = tools.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"edit_file".to_string()));
        assert!(names.contains(&"list_dir".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"present_for_review".to_string()));
        assert!(names.contains(&"ask_user_text".to_string()));
        assert!(names.contains(&"ask_user_choice".to_string()));
        assert!(names.contains(&"ask_user_confirm".to_string()));
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
    async fn ask_user_tools_not_handled_by_builtin_execute() {
        // ask_user_* tools are intercepted upstream (worker.rs/foreground.rs),
        // exactly like present_for_review — execute() must never run them.
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        for name in ["ask_user_text", "ask_user_choice", "ask_user_confirm"] {
            let result = tools.execute(name, serde_json::json!({})).await;
            assert!(result.contains("Unknown built-in tool"));
        }
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
    fn names_returns_ten_entries() {
        let dir = std::env::temp_dir();
        let tools = make_tools(&dir);
        assert_eq!(tools.names().len(), 10);
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
        // More ".." segments than the workdir itself has components, so
        // `normalized.pop()` fails on an already-empty path (distinct from
        // the "popped below workdir root" case covered by
        // resolve_rejects_path_escape). Which of the two bail messages
        // fires ("escapes the working directory" vs "would escape the
        // working directory") can depend on how many path components the
        // platform's own temp-dir path decomposes into (e.g. Windows'
        // drive-prefix + UNC handling), so this only asserts the common
        // "escape" substring both share, not the exact message -- either
        // is an equally correct rejection.
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let deep_traversal = "../".repeat(64) + "etc/passwd";
        let result = tools.resolve(&deep_traversal);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("escape"));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_write_failure_after_successful_match() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let file_path = dir.path().join("ro.txt");
        fs::write(&file_path, "hello world").unwrap();

        // Make the file read-only so the read succeeds but the write-back fails.
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&file_path, perms).unwrap();

        let result = tools
            .execute(
                "edit_file",
                json!({"path": "ro.txt", "old_str": "hello", "new_str": "goodbye"}),
            )
            .await;

        // Restore permissions so tempdir cleanup can remove the file.
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_mode(0o644);
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

    // `detect_shell()` resolves to `cmd.exe /C` on Windows, which doesn't
    // understand Unix shell syntax (`;` as a command separator, `1>&2`
    // redirection the way `sh`/`bash` do) -- `cmd.exe` treats the whole
    // string as one literal `echo` argument instead, so the command
    // "succeeds" with no [exit code 1] at all. Gated `#[cfg(unix)]` rather
    // than attempting an unverified `cmd.exe`-syntax equivalent (this
    // session already hit multiple real Windows CI failures from
    // insufficiently-verified platform-specific test code; not worth
    // risking a new one here without access to a real Windows run to
    // confirm the exact `cmd.exe` redirection/chaining syntax first).
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_failing_command_reports_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let tools = make_tools(dir.path());
        let result = tools
            .execute(
                "shell",
                json!({"command": "echo out-line; echo err-line 1>&2; exit 1"}),
            )
            .await;
        assert!(result.contains("[exit code 1]"));
        assert!(result.contains("stdout:"));
        assert!(result.contains("out-line"));
        assert!(result.contains("stderr:"));
        assert!(result.contains("err-line"));
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

    // Serialize tests that read or write $SHELL to prevent races.
    #[cfg(not(windows))]
    static SHELL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn detect_shell_returns_valid_shell() {
        #[cfg(not(windows))]
        let _g = SHELL_ENV_LOCK.lock().unwrap();
        let (shell, flag) = BuiltinTools::detect_shell();
        assert!(!shell.is_empty());
        assert!(!flag.is_empty());
        #[cfg(not(windows))]
        assert_eq!(flag, "-c");
    }

    /// Forces `detect_shell()` to exercise the real `shell_exists` closure by
    /// temporarily setting $SHELL to an unrecognized path, causing the candidate
    /// loop (and the closure) to be reached.
    #[cfg(not(windows))]
    #[test]
    fn detect_shell_queries_real_filesystem_for_unrecognized_shell() {
        let _g = SHELL_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("SHELL", "/opt/not-a-recognized-shell");
        }
        let (shell, flag) = BuiltinTools::detect_shell();
        // Restore: set SHELL to the found shell (always a valid shell on Unix)
        unsafe {
            std::env::set_var("SHELL", shell);
        }
        assert_eq!(flag, "-c");
        assert!([
            "/bin/bash",
            "/usr/bin/bash",
            "/bin/zsh",
            "/usr/bin/zsh",
            "/bin/sh"
        ]
        .contains(&shell));
    }

    // ── detect_shell_impl() — inject env and filesystem for full branch coverage ──

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_zsh_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/local/bin/zsh".to_string()), |_| false);
        assert_eq!(shell, "/usr/local/bin/zsh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_bash_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/local/bin/bash".to_string()), |_| false);
        assert_eq!(shell, "/usr/local/bin/bash");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_sh_from_env() {
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/usr/bin/sh".to_string()), |_| false);
        assert_eq!(shell, "/usr/bin/sh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_falls_through_when_env_unrecognized() {
        // /opt/fish doesn't end with /zsh, /bash, or /sh → falls to candidate loop
        let (shell, flag) =
            BuiltinTools::detect_shell_impl(Some("/opt/fish".to_string()), |s| s == "/bin/bash");
        assert_eq!(shell, "/bin/bash");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_skips_missing_candidates_and_finds_zsh() {
        // bash paths return false; /bin/zsh exists — covers shell_exists false branch
        let (shell, flag) = BuiltinTools::detect_shell_impl(None, |s| s == "/bin/zsh");
        assert_eq!(shell, "/bin/zsh");
        assert_eq!(flag, "-c");
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_shell_impl_returns_last_resort_when_nothing_exists() {
        let (shell, flag) = BuiltinTools::detect_shell_impl(None, |_| false);
        assert_eq!(shell, "sh");
        assert_eq!(flag, "-c");
    }
}
