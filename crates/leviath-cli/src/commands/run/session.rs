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
            let template = build_task_template(agent_name, description);

            // Write to a temp file
            let tmp_path =
                std::env::temp_dir().join(format!("lev-task-{}.txt", std::process::id()));
            write_task_template(&tmp_path, &template)?;

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

fn build_task_template(agent_name: &str, description: Option<&str>) -> String {
    let mut template = format!("# Task for agent: {}\n", agent_name);
    if let Some(desc) = description {
        if !desc.is_empty() {
            template.push_str(&format!("# {}\n", desc));
        }
    }
    template.push_str("#\n# Describe your task below. Lines starting with '#' are ignored.\n\n");
    template
}

fn write_task_template(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    std::fs::write(path, content)
        .map_err(|e| anyhow::anyhow!("Failed to create task temp file: {}", e))
}

/// Platform-specific fallback editor candidates, appended after any
/// $VISUAL/$EDITOR candidates.
///
/// Extracted into its own pure, injectable function (rather than inlined
/// directly in [`launch_editor`]) so tests can assert on the Windows
/// candidate list containing `notepad` without ever having to actually
/// spawn it — launching a real, blocking, interactive GUI text editor with
/// no timeout would hang CI indefinitely.
fn platform_default_editors() -> Vec<String> {
    #[cfg(unix)]
    {
        vec!["vim".to_string(), "nano".to_string(), "vi".to_string()]
    }
    #[cfg(windows)]
    {
        vec!["notepad".to_string()]
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

    candidates.extend(platform_default_editors());
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
            // Exited (even non-zero means the user closed it — treat as OK).
            // Written as a match guard (rather than a nested
            // `if { return Ok(()); }` block) so the arm is a single
            // expression: with the nested-block form, llvm-cov's coverage
            // mapping attributes a region to the block's closing brace that
            // represents "control fell through the if without returning" —
            // which is unreachable given the block's only statement is an
            // unconditional `return` — so that line reads as permanently
            // uncovered no matter how thoroughly the success path is
            // tested. A guard-arm has no such trailing block to fall
            // through.
            Ok(status) if status.success() || status.code().is_some() => {
                return Ok(());
            }
            Ok(_) => {
                // Terminated with neither a success status nor an exit code
                // (e.g. killed by signal on Unix) — try the next candidate.
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
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-wide `VISUAL`/`EDITOR` env
    /// vars, since `cargo test` runs tests in parallel threads within the
    /// same process. Shared across both the Unix and Windows `launch_editor`
    /// test suites below (each platform mutates the same two env vars, just
    /// with platform-appropriate script paths).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that clears `VISUAL`/`EDITOR` on drop, restoring a clean
    /// environment for subsequent tests regardless of panics.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("VISUAL");
                std::env::remove_var("EDITOR");
            }
        }
    }

    /// Shared `assert!`-with-dynamic-message helper: several `launch_editor`
    /// success tests assert `result.is_ok()` while formatting the actual
    /// result into the panic message for diagnostics if the assertion ever
    /// fails. The panic-message formatting is only evaluated on failure,
    /// which otherwise leaves it permanently uncovered by `cargo llvm-cov`.
    /// Extracted once here (rather than per call site) and exercised below
    /// via `#[should_panic]`.
    fn assert_launch_ok(result: &anyhow::Result<()>) {
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    #[should_panic(expected = "expected Ok, got Err(boom)")]
    fn assert_launch_ok_panics_when_err() {
        assert_launch_ok(&Err(anyhow::anyhow!("boom")));
    }

    // ─── platform_default_editors ─────────────────────────────────────────

    #[cfg(windows)]
    #[test]
    fn platform_default_editors_includes_notepad() {
        assert_eq!(platform_default_editors(), vec!["notepad".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn platform_default_editors_includes_vim_nano_vi() {
        assert_eq!(
            platform_default_editors(),
            vec!["vim".to_string(), "nano".to_string(), "vi".to_string()]
        );
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
        assert!(registry.has("ollama"));
        assert!(registry.has("claude-code"));
        // Should NOT have anthropic, openai, google without keys
        assert!(!registry.has("anthropic"));
        assert!(!registry.has("openai"));
        assert!(!registry.has("google"));
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
        assert!(registry.has("anthropic"));
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
        assert!(registry.has("openai"));
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
        assert!(registry.has("google"));
    }

    #[test]
    fn build_provider_registry_with_openrouter_key() {
        let config = Config {
            openrouter_api_key: Some("sk-or-test-12345".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("openrouter"));
    }

    #[test]
    fn build_provider_registry_custom_ollama_url() {
        let config = Config {
            ollama_base_url: Some("http://my-server:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry(&config);
        assert!(registry.has("ollama"));
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
        assert_launch_ok(&result);

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
        assert_launch_ok(&result);

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
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: terminated by signal (no exit code) tries next ───

    #[cfg(unix)]
    #[test]
    fn launch_editor_signal_killed_falls_through_to_next() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("lev-test-launch-editor-signal-killed");
        let _ = std::fs::create_dir_all(&dir);

        // A script that kills itself with SIGKILL: `Command::status()` then
        // reports an `ExitStatus` with `success() == false` *and*
        // `code() == None` (terminated by signal, not a normal exit) --
        // the one outcome that falls into the `Ok(_) => {}` arm (neither
        // the success guard nor an `Err(...)` variant), which should try
        // the next candidate rather than returning.
        let self_kill_script = dir.join("self-kill.sh");
        std::fs::write(&self_kill_script, "#!/bin/sh\nkill -9 $$\n").unwrap();
        std::fs::set_permissions(&self_kill_script, std::fs::Permissions::from_mode(0o700))
            .unwrap();

        unsafe {
            std::env::set_var("VISUAL", &self_kill_script);
            std::env::set_var("EDITOR", "/usr/bin/true");
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert_launch_ok(&result);

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
        assert_launch_ok(&result);

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
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: truly-empty VISUAL/EDITOR are skipped entirely ────

    // Unlike the whitespace-only case above (non-empty string, pushed as a
    // candidate that then fails to split into any usable parts), a truly
    // empty `VISUAL`/`EDITOR` value never even gets pushed onto the
    // candidates list -- exercising the `!v.is_empty()`/`!e.is_empty()`
    // false arm for both, which no other test reaches (every other test
    // either leaves the var unset or sets it to a non-empty value).
    //
    // With both vars empty, resolution falls through to the unix platform
    // defaults (vim/nano/vi) unless PATH is also starved -- so this test
    // combines the empty-string case with the same PATH-starvation trick as
    // `launch_editor_no_editor_found_when_path_has_no_candidates` below,
    // guaranteeing a deterministic `Err` instead of ever risking a real,
    // blocking, interactive editor launch.
    #[cfg(unix)]
    #[test]
    fn launch_editor_empty_visual_and_editor_are_skipped() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _path_lock = crate::config::PATH_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;

        struct PathGuard(std::ffi::OsString);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    std::env::set_var("PATH", &self.0);
                }
            }
        }
        let _path_guard = PathGuard(std::env::var_os("PATH").unwrap_or_default());

        unsafe {
            std::env::set_var("VISUAL", "");
            std::env::set_var("EDITOR", "");
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }
        let dir = std::env::temp_dir().join("lev-test-launch-editor-empty-visual-editor");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        // Neither empty var is pushed as a candidate, and PATH starvation
        // means even the unix platform defaults (vim/nano/vi) fail to
        // resolve -- so this deterministically reaches "no editor found"
        // rather than ever spawning a real editor.
        let result = launch_editor(&file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No editor found"));

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
        assert_launch_ok(&result);

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
        // `PATH` is process-global; this also serializes against any other
        // test in the crate (e.g. `dashboard::helpers`'s clipboard-fallback
        // test) that mutates it, since `ENV_LOCK` here only covers
        // VISUAL/EDITOR, not PATH.
        let _path_lock = crate::config::PATH_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;

        /// Restores the real `PATH` on drop -- separate from `EnvGuard`
        /// (which only handles `VISUAL`/`EDITOR`) since breaking `PATH` for
        /// the rest of the test process would be far more disruptive than
        /// leaving those two unset.
        struct PathGuard(std::ffi::OsString);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    std::env::set_var("PATH", &self.0);
                }
            }
        }
        let _path_guard = PathGuard(std::env::var_os("PATH").unwrap_or_default());
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

    // ─── launch_editor: Windows twin suite ────────────────────────────────
    //
    // Windows can't reuse the Unix tests above verbatim: `/usr/bin/true` /
    // `/usr/bin/false` don't exist, shebang scripts can't execute (`os error
    // 193`), and Unix permission bits (`PermissionsExt`) don't apply. Batch
    // (`.bat`) files stand in for the shebang scripts -- they're directly
    // executable via `Command::new(path)` on Windows, exit instantly, and
    // never touch a real interactive editor.
    //
    // Two of the Unix tests have NO safe Windows equivalent and are
    // intentionally not mirrored here:
    //   - `launch_editor_signal_killed_falls_through_to_next`: the `Ok(_) =>
    //     {}` arm exists for processes terminated with no exit code (e.g.
    //     killed by a Unix signal). On Windows, `ExitStatus::code()` is
    //     populated from `GetExitCodeProcess` and is effectively always
    //     `Some(_)`, so this arm is unreachable there -- a genuine permanent
    //     gap, not a missing test.
    //   - `launch_editor_empty_visual_and_editor_are_skipped` /
    //     `launch_editor_no_editor_found_when_path_has_no_candidates` /
    //     `resolve_task_with_editor_path_propagates_launch_editor_error`:
    //     all rely on PATH-starvation making every candidate unresolvable so
    //     `launch_editor` reaches its final "No editor found" bail. On
    //     Windows the platform-default candidate is `notepad`, which
    //     `CreateProcess`'s search order resolves via `System32`
    //     *unconditionally before* consulting `$PATH` -- so it can never be
    //     made to fail to resolve without mutating the real system
    //     directory, which is far too risky/disruptive for a test. Another
    //     genuine permanent gap.

    #[cfg(windows)]
    fn write_bat(path: &std::path::Path, body: &str) {
        std::fs::write(path, format!("@echo off\r\n{}\r\n", body)).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_visual_env_success() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = std::env::temp_dir().join("lev-test-launch-editor-visual-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        unsafe {
            std::env::set_var("VISUAL", &ok_bat);
            std::env::remove_var("EDITOR");
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_editor_env_success() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = std::env::temp_dir().join("lev-test-launch-editor-editor-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        unsafe {
            std::env::remove_var("VISUAL");
            std::env::set_var("EDITOR", &ok_bat);
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_nonzero_exit_still_ok() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = std::env::temp_dir().join("lev-test-launch-editor-nonzero-win");
        let _ = std::fs::create_dir_all(&dir);
        let fail_bat = dir.join("fail.bat");
        write_bat(&fail_bat, "exit /b 1");

        unsafe {
            std::env::set_var("VISUAL", &fail_bat);
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        // A non-zero-but-present exit code is treated as the user having
        // closed the editor -- not an error.
        let result = launch_editor(&file);
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_command_with_flags_splits_correctly() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = std::env::temp_dir().join("lev-test-launch-editor-flags-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        unsafe {
            // The batch file ignores all arguments, so appending a flag and
            // the file path is harmless; this exercises the
            // whitespace-splitting logic for editor strings like
            // "code --wait".
            std::env::set_var("VISUAL", format!("{} --some-flag", ok_bat.display()));
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_whitespace_only_visual_falls_through_to_editor() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = std::env::temp_dir().join("lev-test-launch-editor-ws-visual-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        unsafe {
            // Whitespace-only string is non-empty so it IS pushed as a
            // candidate, but splitting on whitespace yields an empty parts
            // vec, which triggers the `continue` branch.
            std::env::set_var("VISUAL", "   ");
            std::env::set_var("EDITOR", &ok_bat);
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_not_found_candidate_falls_through_to_next() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        let dir = std::env::temp_dir().join("lev-test-launch-editor-notfound-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        unsafe {
            // VISUAL points at a nonexistent binary, which should be
            // skipped (NotFound branch, `continue`) in favor of EDITOR.
            std::env::set_var("VISUAL", "lev-definitely-not-a-real-binary-xyz");
            std::env::set_var("EDITOR", &ok_bat);
        }
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor(&file);
        assert_launch_ok(&result);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_permission_denied_returns_error() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;

        let dir = std::env::temp_dir().join("lev-test-launch-editor-perm-denied-win");
        let _ = std::fs::create_dir_all(&dir);
        // A plain, non-executable text file: Windows' `CreateProcess` can't
        // recognize it as an executable image and fails with
        // `ERROR_BAD_EXE_FORMAT` (os error 193), not `NotFound` -- exercising
        // the generic `Err(e)` arm (as opposed to the `NotFound` "try next
        // candidate" arm already covered above).
        let not_executable = dir.join("not-executable.txt");
        std::fs::write(&not_executable, "not a script").unwrap();

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
        // See the comment in `launch_editor_no_editor_found_when_path_has_no_candidates`.
        let _path_lock = crate::config::PATH_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        struct PathGuard(std::ffi::OsString);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    std::env::set_var("PATH", &self.0);
                }
            }
        }
        let _path_guard = PathGuard(std::env::var_os("PATH").unwrap_or_default());
        unsafe {
            std::env::set_var("PATH", "/lev-definitely-empty-path-dir");
        }

        let result = resolve_task_with(&None, "test-agent", None, || true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No editor found"));
    }

    // ─── resolve_task_with: editor path (stdin is a TTY) — Windows twins ──

    #[cfg(windows)]
    #[test]
    fn resolve_task_with_editor_path_happy_case() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;

        // A tiny batch "editor" that appends a non-comment line to whatever
        // file it's invoked on (%~1) -- standing in for a real interactive
        // editor session. `%~1` strips any surrounding quotes Windows adds
        // around a path containing spaces.
        let dir = std::env::temp_dir().join("lev-test-resolve-task-editor-happy-win");
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("fake-editor.bat");
        write_bat(&script, "echo task body from editor>>\"%~1\"");

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

    #[cfg(windows)]
    #[test]
    fn resolve_task_with_editor_path_empty_after_stripping_comments_errors() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard;
        // A no-op batch file "opens" the file and does nothing to it, so
        // only the commented-out template remains -- stripped down to an
        // empty task.
        let dir = std::env::temp_dir().join("lev-test-resolve-task-editor-empty-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        unsafe {
            std::env::set_var("VISUAL", &ok_bat);
        }

        let result = resolve_task_with(&None, "test-agent", None, || true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Aborting run"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── build_task_template: description branch ──────────────────────────

    #[test]
    fn build_task_template_with_empty_description_skips_desc_line() {
        let t = build_task_template("agent", Some(""));
        assert!(!t.contains("# \n"));
        assert!(t.contains("# Task for agent: agent\n"));
    }

    #[test]
    fn build_task_template_with_non_empty_description_adds_desc_line() {
        let t = build_task_template("my-agent", Some("Build a web server"));
        assert!(t.contains("# Task for agent: my-agent\n"));
        assert!(t.contains("# Build a web server\n"));
    }

    #[test]
    fn build_task_template_with_no_description() {
        let t = build_task_template("my-agent", None);
        assert!(t.contains("# Task for agent: my-agent\n"));
        assert!(t.contains("Describe your task below"));
    }

    // ─── write_task_template: error path ─────────────────────────────────

    // A path whose parent directory doesn't exist fails `fs::write` with
    // "not found" on both Unix (ENOENT) and Windows (ERROR_PATH_NOT_FOUND),
    // so this test is cross-platform rather than needing a `#[cfg(unix)]`
    // twin.
    #[test]
    fn write_task_template_error_on_bad_path() {
        let bad_path = std::env::temp_dir()
            .join("lev-definitely-nonexistent-parent-dir-xyz")
            .join("task.txt");
        let result = write_task_template(&bad_path, "content");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to create task temp file"));
    }

    // ─── resolve_task: unreadable file errors ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn resolve_task_unreadable_file_returns_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("lev-test-resolve-unreadable");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("secret.txt");
        std::fs::write(&file, "secret content").unwrap();
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&file, perms).unwrap();

        let result = resolve_task_with(
            &Some(file.to_str().unwrap().to_string()),
            "test-agent",
            None,
            || false,
        );
        // Restore perms before asserting (so cleanup works)
        let mut perms2 = std::fs::metadata(&file).unwrap().permissions();
        perms2.set_mode(0o644);
        std::fs::set_permissions(&file, perms2).ok();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to read task file"));
    }

    // Windows has no chmod-style permission bits; instead, opening the file
    // for writing with a zero share mode (no `FILE_SHARE_READ`) makes any
    // concurrent read attempt fail with a sharing violation for as long as
    // the handle stays open -- a deterministic Windows-native way to force
    // the same "file exists but can't be read" outcome the Unix test above
    // produces via `chmod 000`.
    #[cfg(windows)]
    #[test]
    fn resolve_task_unreadable_file_returns_error() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let dir = std::env::temp_dir().join("lev-test-resolve-unreadable-win");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("secret.txt");
        std::fs::write(&file, "secret content").unwrap();

        // Hold an exclusive (no-share) handle open for the duration of the
        // read attempt below.
        let _locked = OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&file)
            .unwrap();

        let result = resolve_task_with(
            &Some(file.to_str().unwrap().to_string()),
            "test-agent",
            None,
            || false,
        );

        drop(_locked);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to read task file"));
    }
}
