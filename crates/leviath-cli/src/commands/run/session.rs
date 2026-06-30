//! Session setup: task resolution, editor launching, engine setup.

use crate::config::Config;
use leviath_runtime::ProviderRegistry;
use std::sync::Arc;

/// Resolve the task string from a CLI argument.
///
/// - `Some(s)` where `s` is an existing file path → read file contents.
/// - `Some(s)` otherwise → use `s` as a literal prompt.
/// - `None` when stdin is not a TTY → error.
/// - `None` when stdin is a TTY → launch the user's editor on a temp prompt file.
pub fn resolve_task(
    arg: &Option<String>,
    agent_name: &str,
    description: Option<&str>,
) -> anyhow::Result<String> {
    match arg {
        Some(s) => {
            let p = std::path::Path::new(s);
            if p.is_file() {
                let content = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("Failed to read task file '{}': {}", s, e))?;
                let trimmed = content.trim().to_string();
                if trimmed.is_empty() {
                    anyhow::bail!("Task file '{}' is empty.", s);
                }
                return Ok(trimmed);
            }
            Ok(s.clone())
        }
        None => {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "No task provided. Pass --task \"<prompt>\" or --task <file>.\n\
                     (stdin is not a TTY, so the interactive editor cannot be used)"
                );
            }

            // Build a commented template file for the editor
            let mut template = format!("# Task for agent: {}\n", agent_name);
            if let Some(desc) = description {
                if !desc.is_empty() {
                    template.push_str(&format!("# {}\n", desc));
                }
            }
            template.push_str(
                "#\n# Describe your task below. Lines starting with '#' are ignored.\n\n",
            );

            // Write to a temp file
            let tmp_path =
                std::env::temp_dir().join(format!("lev-task-{}.txt", std::process::id()));
            std::fs::write(&tmp_path, &template)
                .map_err(|e| anyhow::anyhow!("Failed to create task temp file: {}", e))?;

            // Launch the editor (exits only when the user closes it)
            let result = launch_editor(&tmp_path);
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp_path);
            result?;

            // Strip comment lines and trim
            let task: String = content
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();

            if task.is_empty() {
                anyhow::bail!("Aborting run: empty task.");
            }
            Ok(task)
        }
    }
}

/// Launch the user's preferred editor on `path` and wait for it to exit.
///
/// Editor resolution order: $VISUAL → $EDITOR → platform default.
/// Platform defaults: Unix tries `vim` then `nano`; Windows uses `notepad`.
fn launch_editor(path: &std::path::Path) -> anyhow::Result<()> {
    use std::process::Command;

    // Resolve editor candidates in priority order
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(v) = std::env::var("VISUAL") {
        if !v.is_empty() {
            candidates.push(v);
        }
    }
    if let Ok(e) = std::env::var("EDITOR") {
        if !e.is_empty() {
            candidates.push(e);
        }
    }

    #[cfg(unix)]
    {
        candidates.push("vim".to_string());
        candidates.push("nano".to_string());
        candidates.push("vi".to_string());
    }
    #[cfg(windows)]
    {
        candidates.push("notepad".to_string());
    }
    // Final fallback
    if candidates.is_empty() {
        candidates.push("nano".to_string());
    }

    let path_str = path.to_string_lossy();

    for editor in &candidates {
        // Handle editor strings that may include flags (e.g. "code --wait")
        let parts: Vec<&str> = editor.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let mut cmd = Command::new(parts[0]);
        for arg in &parts[1..] {
            cmd.arg(arg);
        }
        cmd.arg(path_str.as_ref());

        match cmd.status() {
            Ok(status) => {
                if status.success() || status.code().is_some() {
                    // Exited (even non-zero means the user closed it — treat as OK)
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Try next candidate
                continue;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to launch editor '{}': {}",
                    editor,
                    e
                ));
            }
        }
    }

    anyhow::bail!("No editor found. Set $VISUAL or $EDITOR, or install vim/nano/notepad.")
}

/// Build a ProviderRegistry from Config.
pub fn build_provider_registry(config: &Config) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    if let Some(ref key) = config.providers.anthropic_api_key {
        registry.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    if let Some(ref key) = config.providers.openai_api_key {
        registry.register(
            "openai".to_string(),
            Arc::new(leviath_providers::OpenAIProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    if let Some(ref key) = config.providers.google_api_key {
        registry.register(
            "google".to_string(),
            Arc::new(leviath_providers::GeminiProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    if let Some(ref key) = config.openrouter_api_key {
        registry.register(
            "openrouter".to_string(),
            Arc::new(leviath_providers::OpenRouterProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    let ollama_url = config
        .ollama_base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    registry.register(
        "ollama".to_string(),
        Arc::new(leviath_providers::OllamaProvider::with_overrides(
            ollama_url.to_string(),
            config.model_capabilities.clone(),
        )),
    );

    // Claude Code provider (no API key needed - uses claude CLI subscription)
    registry.register(
        "claude-code".to_string(),
        Arc::new(leviath_providers::ClaudeCodeProvider::new()),
    );

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_task_with_literal_string() {
        let result = resolve_task(&Some("do something".to_string()), "test", None);
        assert_eq!(result.unwrap(), "do something");
    }

    #[test]
    fn resolve_task_with_file_path() {
        let dir = std::env::temp_dir().join("lev-test-resolve-task");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("task.txt");
        std::fs::write(&file, "task from file\n").unwrap();

        let result = resolve_task(&Some(file.to_str().unwrap().to_string()), "test", None);
        assert_eq!(result.unwrap(), "task from file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_task_with_empty_file_errors() {
        let dir = std::env::temp_dir().join("lev-test-resolve-empty");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("empty.txt");
        std::fs::write(&file, "   \n  ").unwrap();

        let result = resolve_task(&Some(file.to_str().unwrap().to_string()), "test", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_task_nonexistent_file_used_as_literal() {
        let result = resolve_task(
            &Some("/nonexistent/path/do_something".to_string()),
            "test",
            None,
        );
        // Path doesn't exist as a file, so it's treated as a literal string
        assert_eq!(result.unwrap(), "/nonexistent/path/do_something");
    }
}
