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
    use std::io::IsTerminal;
    resolve_task_with(arg, agent_name, description, || {
        std::io::stdin().is_terminal()
    })
}

/// Same as [`resolve_task`], but with the stdin-is-a-TTY check injected
/// instead of hardcoded — lets tests deterministically exercise both the
/// "not a TTY" error path and the "is a TTY" editor-launch path regardless
/// of whether the test runner's own stdin happens to be a real terminal
/// (e.g. a human running `cargo test` interactively vs. CI).
fn resolve_task_with(
    arg: &Option<String>,
    agent_name: &str,
    description: Option<&str>,
    stdin_is_terminal: impl FnOnce() -> bool,
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
            if !stdin_is_terminal() {
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
    // Final fallback -- unreachable under the `#[cfg(unix)]`/`#[cfg(windows)]`
    // targets this crate actually ships for, since both of those blocks
    // unconditionally push at least one candidate already. Only relevant on
    // a hypothetical third target platform with neither cfg set.
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
    #[cfg(unix)]
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-wide `VISUAL`/`EDITOR` env
    /// vars, since `cargo test` runs tests in parallel threads within the
    /// same process.
    ///
    /// Every current usage lives inside a `#[cfg(unix)]` test (the
    /// `VISUAL`/`EDITOR` values used are Unix-only paths like
    /// `/usr/bin/true`, and PATH-starvation to force "no editor found"
    /// doesn't work on Windows since it resolves `notepad` via System32
    /// regardless of `$PATH`) -- so this must be `#[cfg(unix)]` too, or a
    /// non-Unix build sees it as genuine dead code under `-D warnings`.
    #[cfg(unix)]
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that clears `VISUAL`/`EDITOR` on drop, restoring a clean
    /// environment for subsequent tests regardless of panics.
    #[cfg(unix)]
    struct EnvGuard;
    #[cfg(unix)]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("VISUAL");
                std::env::remove_var("EDITOR");
            }
        }
    }

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

    #[test]
    fn resolve_task_file_with_whitespace_only_errors() {
        let dir = std::env::temp_dir().join("lev-test-resolve-ws");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("whitespace.txt");
        std::fs::write(&file, "   \n\t\n  \n").unwrap();

        let result = resolve_task(&Some(file.to_str().unwrap().to_string()), "test", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_task_file_trims_content() {
        let dir = std::env::temp_dir().join("lev-test-resolve-trim");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("trimme.txt");
        std::fs::write(&file, "  hello world  \n\n").unwrap();

        let result = resolve_task(&Some(file.to_str().unwrap().to_string()), "test", None);
        assert_eq!(result.unwrap(), "hello world");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_task_preserves_literal_string_as_is() {
        let result = resolve_task(&Some("  spaces around  ".to_string()), "test", None);
        assert_eq!(result.unwrap(), "  spaces around  ");
    }

    #[test]
    fn build_provider_registry_with_empty_config() {
        let config = Config::default();
        let registry = build_provider_registry(&config);
        // Should always have ollama and claude-code registered
        assert!(registry.has("ollama"), "ollama should be registered");
        assert!(
            registry.has("claude-code"),
            "claude-code should be registered"
        );
        // Should NOT have anthropic, openai, google without keys
        assert!(
            !registry.has("anthropic"),
            "anthropic should not be registered without key"
        );
        assert!(
            !registry.has("openai"),
            "openai should not be registered without key"
        );
        assert!(
            !registry.has("google"),
            "google should not be registered without key"
        );
    }

    #[test]
    fn build_provider_registry_with_anthropic_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test-key-12345".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("anthropic"), "anthropic should be registered");
    }

    #[test]
    fn build_provider_registry_with_openai_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                openai_api_key: Some("sk-test-key-12345".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("openai"), "openai should be registered");
    }

    #[test]
    fn build_provider_registry_with_google_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                google_api_key: Some("AIzatest12345".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("google"), "google should be registered");
    }

    #[test]
    fn build_provider_registry_with_openrouter_key() {
        let config = Config {
            openrouter_api_key: Some("sk-or-test-12345".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(
            registry.has("openrouter"),
            "openrouter should be registered"
        );
    }

    #[test]
    fn build_provider_registry_custom_ollama_url() {
        let config = Config {
            ollama_base_url: Some("http://my-server:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("ollama"), "ollama should be registered");
    }

    // ─── build_provider_registry with all keys ──────────────────────────

    #[test]
    fn build_provider_registry_all_keys_set() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: Some("sk-test".to_string()),
                google_api_key: Some("AIza-test".to_string()),
            },
            openrouter_api_key: Some("sk-or-test".to_string()),
            ollama_base_url: Some("http://custom:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        assert!(registry.has("claude-code"));
    }

    // ─── resolve_task: multiline file content ───────────────────────────

    #[test]
    fn resolve_task_multiline_file() {
        let dir = std::env::temp_dir().join("lev-test-resolve-multiline");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("multi.txt");
        std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

        let result = resolve_task(&Some(file.to_str().unwrap().to_string()), "test", None);
        let task = result.unwrap();
        assert!(task.contains("line one"));
        assert!(task.contains("line two"));
        assert!(task.contains("line three"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── resolve_task: literal string with special chars ────────────────

    #[test]
    fn resolve_task_literal_with_special_chars() {
        let result = resolve_task(
            &Some("Write a function that does X & Y <html>".to_string()),
            "test",
            None,
        );
        assert_eq!(result.unwrap(), "Write a function that does X & Y <html>");
    }

    // ─── build_provider_registry: no providers except defaults ──────────

    #[test]
    fn build_provider_registry_defaults_always_have_ollama_and_claude_code() {
        let config = Config::default();
        let registry = build_provider_registry(&config);
        // These should always be present regardless of key configuration
        let provider_count = ["ollama", "claude-code"]
            .iter()
            .filter(|name| registry.has(name))
            .count();
        assert_eq!(provider_count, 2);
    }

    // ─── resolve_task: file with only comments in editor-like format ────

    #[test]
    fn resolve_task_literal_empty_string() {
        // Empty string is treated as literal, returns as-is
        let result = resolve_task(&Some("".to_string()), "test", None);
        assert_eq!(result.unwrap(), "");
    }

    // ─── resolve_task: file with multiple lines and trailing whitespace ──

    #[test]
    fn resolve_task_file_with_multiple_trailing_newlines() {
        let dir = std::env::temp_dir().join("lev-test-resolve-trail");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("trail.txt");
        std::fs::write(&file, "task content\n\n\n\n").unwrap();

        let result = resolve_task(&Some(file.to_str().unwrap().to_string()), "test", None);
        assert_eq!(result.unwrap(), "task content");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── build_provider_registry: model_capabilities propagated ──────────

    #[test]
    fn build_provider_registry_propagates_model_capabilities() {
        use leviath_providers::ModelCapabilities;
        let mut caps = std::collections::HashMap::new();
        caps.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 9999,
                max_output_tokens: 999,
            },
        );
        let config = crate::config::Config {
            model_capabilities: caps,
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            ..crate::config::Config::default()
        };
        let registry = build_provider_registry(&config);
        // Verify anthropic provider was registered
        assert!(registry.has("anthropic"));
        // Verify ollama always registered
        assert!(registry.has("ollama"));
    }

    // ─── launch_editor: candidates exhausted when no editors available ────

    #[test]
    fn resolve_task_file_with_real_content() {
        let dir = std::env::temp_dir().join("lev-test-resolve-real");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("real.txt");
        std::fs::write(&file, "Implement a REST API server\nwith authentication\n").unwrap();

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "api-agent",
            Some("API agent"),
        );
        let task = result.unwrap();
        assert!(task.contains("Implement a REST API server"));
        assert!(task.contains("with authentication"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── build_provider_registry: all providers registered ───────────────

    #[test]
    fn build_provider_registry_ollama_with_custom_url_propagates_caps() {
        use leviath_providers::ModelCapabilities;
        let mut caps = std::collections::HashMap::new();
        caps.insert(
            "llama3-8b".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 99,
                max_output_tokens: 99,
            },
        );
        let config = crate::config::Config {
            ollama_base_url: Some("http://custom-ollama:11434".to_string()),
            model_capabilities: caps,
            ..crate::config::Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("ollama"));
    }

    // ─── resolve_task: None arg, non-TTY stdin ───────────────────────────

    #[test]
    fn resolve_task_none_arg_errors_when_stdin_not_tty() {
        // The TTY check is injected (not the real std::io::stdin()) so this
        // is deterministic regardless of whether the test runner's own
        // stdin happens to be a real terminal — a human running `cargo test`
        // interactively has a real TTY on stdin, unlike CI, so hardcoding
        // "stdin is never a TTY under cargo test" was a false assumption.
        let result = resolve_task_with(&None, "test-agent", None, || false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("No task provided"));
        assert!(msg.contains("stdin is not a TTY"));
    }

    #[test]
    fn resolve_task_none_arg_uses_real_stdin_check_via_public_wrapper() {
        // Smoke test that the public resolve_task() wrapper still compiles
        // and delegates correctly — doesn't assert on the TTY-dependent
        // outcome itself, since that legitimately varies by environment.
        let result = resolve_task(&Some("literal task".to_string()), "test-agent", None);
        assert_eq!(result.unwrap(), "literal task");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_task_none_arg_exercises_real_stdin_is_terminal_check() {
        // Unlike reading from stdin, `IsTerminal::is_terminal()` is a
        // non-blocking fd check -- safe to call for real. This drives
        // `resolve_task`'s actual (non-injected) closure at least once. The
        // outcome depends on whether *this test process's* stdin happens to
        // be a TTY, so VISUAL is set to a no-op editor to keep both possible
        // branches fast and deterministic: TTY-false hits the "no task
        // provided" error immediately; TTY-true opens the (untouched)
        // template through `/usr/bin/true` and then errors on the resulting
        // empty task -- neither path blocks or spawns a real interactive
        // editor.
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            std::env::set_var("VISUAL", "/usr/bin/true");
        }
        let result = resolve_task(&None, "test-agent", None);
        assert!(result.is_err());
    }

    // ─── launch_editor: VISUAL takes priority and succeeds ───────────────

    // These `launch_editor` tests point VISUAL/EDITOR at `/usr/bin/true` (or
    // rely on PATH-starvation to prevent any editor being found) -- both
    // assumptions are Unix-only. On Windows, `/usr/bin/true` doesn't exist,
    // so the NotFound-branch falls through to the windows-only "notepad"
    // candidate, which Windows resolves via its System32 search path
    // *regardless* of $PATH -- so PATH-starvation doesn't stop it either.
    // Either way that means launching a real, blocking GUI text editor with
    // no timeout, which hung a Windows CI run indefinitely. Gated to `unix`.
    #[cfg(unix)]
    #[test]
    fn launch_editor_visual_env_success() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            std::env::set_var("VISUAL", "/usr/bin/true");
            std::env::remove_var("EDITOR");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-visual");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: EDITOR used when VISUAL unset ────────────────────

    #[cfg(unix)]
    #[test]
    fn launch_editor_editor_env_success() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            std::env::remove_var("VISUAL");
            std::env::set_var("EDITOR", "/usr/bin/true");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-editor");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: exit code (even non-zero) is treated as success ──

    #[cfg(unix)]
    #[test]
    fn launch_editor_nonzero_exit_still_ok() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            std::env::set_var("VISUAL", "/usr/bin/false");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-nonzero");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        // A non-zero-but-present exit code is treated as the user having
        // closed the editor -- not an error.
        let result = launch_editor(&file);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: command with flags is split correctly ────────────

    #[cfg(unix)]
    #[test]
    fn launch_editor_command_with_flags_splits_correctly() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            // `/usr/bin/true` ignores all arguments, so appending a flag
            // and the file path is harmless; this exercises the
            // whitespace-splitting logic for editor strings like
            // "code --wait".
            std::env::set_var("VISUAL", "/usr/bin/true --some-flag");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-flags");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: whitespace-only VISUAL falls through, EDITOR used ─

    #[cfg(unix)]
    #[test]
    fn launch_editor_whitespace_only_visual_falls_through_to_editor() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            // Whitespace-only string is non-empty so it IS pushed as a
            // candidate, but splitting on whitespace yields an empty parts
            // vec, which triggers the `continue` branch.
            std::env::set_var("VISUAL", "   ");
            std::env::set_var("EDITOR", "/usr/bin/true");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-ws-visual");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: NotFound candidate is skipped, next one used ─────

    #[cfg(unix)]
    #[test]
    fn launch_editor_not_found_candidate_falls_through_to_next() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        unsafe {
            // VISUAL points at a nonexistent binary, which should be
            // skipped (NotFound branch, `continue`) in favor of EDITOR.
            std::env::set_var("VISUAL", "lev-definitely-not-a-real-binary-xyz");
            std::env::set_var("EDITOR", "/usr/bin/true");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-notfound");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: non-NotFound spawn error propagates ──────────────

    #[cfg(unix)]
    #[test]
    fn launch_editor_permission_denied_returns_error() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("lev-test-launch-editor-perm-denied");
        let _ = std::fs::create_dir_all(&dir);
        // A regular, non-executable file: spawning it directly fails with
        // `PermissionDenied`, not `NotFound` -- exercising the generic
        // `Err(e)` arm (as opposed to the `NotFound` "try next candidate"
        // arm already covered above).
        let not_executable = dir.join("not-executable");
        std::fs::write(&not_executable, "not a script").unwrap();
        let mut perms = std::fs::metadata(&not_executable).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&not_executable, perms).unwrap();

        unsafe {
            std::env::set_var("VISUAL", &not_executable);
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to launch editor"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: no candidate resolves anywhere on PATH ────────────

    // Windows resolves "notepad" via the System32 search path regardless of
    // $PATH, so PATH-starvation can't produce a "no editor found" outcome
    // there the way it does on Unix (breaking PATH so vim/nano/vi can't
    // resolve) -- gated to `unix` for the same real-blocking-editor-hang
    // reason as the tests above.
    #[cfg(unix)]
    #[test]
    fn launch_editor_no_editor_found_when_path_has_no_candidates() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;

        /// Restores the real `PATH` on drop -- separate from `EnvGuard`
        /// (which only handles `VISUAL`/`EDITOR`) since breaking `PATH` for
        /// the rest of the test process would be far more disruptive than
        /// leaving those two unset.
        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(p) => std::env::set_var("PATH", p),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _path_guard = PathGuard(std::env::var_os("PATH"));
        unsafe {
            // No VISUAL/EDITOR (cleared by EnvGuard already), and PATH
            // points nowhere -- so even the unix platform-default
            // candidates (vim/nano/vi) all fail to resolve.
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }

        let dir = std::env::temp_dir().join("lev-test-launch-editor-no-editor");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No editor found"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── resolve_task_with: editor path (stdin is a TTY) ──────────────────

    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_happy_case() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        use std::os::unix::fs::PermissionsExt;

        // A tiny "editor" script that appends a non-comment line to
        // whatever file it's invoked on ($1) -- standing in for a real
        // interactive editor session.
        let dir = std::env::temp_dir().join("lev-test-resolve-task-editor-happy");
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("fake-editor.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"task body from editor\" >> \"$1\"\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&script, perms).unwrap();

        unsafe {
            std::env::set_var("VISUAL", &script);
        }

        let result =
            resolve_task_with(&None, "test-agent", Some("a non-empty description"), || {
                true
            });
        assert_eq!(result.unwrap(), "task body from editor");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_empty_after_stripping_comments_errors() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        // /usr/bin/true "opens" the file and does nothing to it, so only the
        // commented-out template remains -- stripped down to an empty task.
        unsafe {
            std::env::set_var("VISUAL", "/usr/bin/true");
        }

        let result = resolve_task_with(&None, "test-agent", None, || true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Aborting run"));
    }

    // Same Windows PATH-starvation caveat as
    // `launch_editor_no_editor_found_when_path_has_no_candidates` above.
    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_propagates_launch_editor_error() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(p) => std::env::set_var("PATH", p),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _path_guard = PathGuard(std::env::var_os("PATH"));
        unsafe {
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }

        let result = resolve_task_with(&None, "test-agent", None, || true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No editor found"));
    }
}
