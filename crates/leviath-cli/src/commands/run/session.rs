//! Session setup: task resolution, editor launching, engine setup.

use crate::config::Config;
use leviath_runtime::ProviderRegistry;

// `ProviderCreds` + `build_provider_registry(&[ProviderCreds])` live in
// `leviath-runtime` (plain data + provider instantiation, no `Config`
// dependency). Re-exported here so `commands::run`'s public re-export and all
// existing call sites keep resolving. The `Config`-based translators
// (`provider_creds_from_config` / `build_provider_registry_from_config`) stay
// below because they need the CLI's `Config`.
pub use leviath_runtime::provider_creds::{ProviderCreds, build_provider_registry};

/// Resolve the task string from a CLI argument.
///
/// - `Some(s)` where `s` is an existing file path → read file contents.
/// - `Some(s)` otherwise → use `s` as a literal prompt.
/// - `None` when stdin is not a TTY → error.
/// - `None` when stdin is a TTY → launch the user's editor on a temp prompt file.
///
/// `stdin_is_terminal` is injected (a `&dyn Fn() -> bool`) rather than probing
/// the real process stdin here, so the library core stays free of direct
/// `std::io::stdin()` access and is fully testable. In production the binary
/// passes `&|| std::io::stdin().is_terminal()`.
pub fn resolve_task(
    arg: &Option<String>,
    agent_name: &str,
    description: Option<&str>,
    stdin_is_terminal: &dyn Fn() -> bool,
) -> anyhow::Result<String> {
    resolve_task_with(arg, agent_name, description, stdin_is_terminal)
}

/// Same as [`resolve_task`], but with the stdin-is-a-TTY check injected
/// instead of hardcoded — lets tests deterministically exercise both the
/// "not a TTY" error path and the "is a TTY" editor-launch path regardless
/// of whether the test runner's own stdin happens to be a real terminal
/// (e.g. a human running `cargo test` interactively vs. CI).
///
/// `stdin_is_terminal` is a trait-object reference (`&dyn Fn`) rather than
/// `impl FnOnce` deliberately: this function is called from many test sites,
/// each passing a distinct closure *type* even when the closures are
/// behaviorally identical (e.g. multiple `|| true`s are still different
/// anonymous types). A generic `impl Trait` parameter gives `rustc` one
/// monomorphization per call site, and `cargo llvm-cov` sometimes reports a
/// region as uncovered for one instantiation even though the union of all
/// instantiations covers every source position -- a confirmed llvm-cov
/// limitation (see `xtask/src/coverage.rs`'s doc comment on generic-function
/// monomorphization). Erasing the closure type with `&dyn Fn` collapses
/// every call site back down to a single instantiation, avoiding that noise
/// entirely.
fn resolve_task_with(
    arg: &Option<String>,
    agent_name: &str,
    description: Option<&str>,
    stdin_is_terminal: &dyn Fn() -> bool,
) -> anyhow::Result<String> {
    resolve_task_with_editor(
        arg,
        agent_name,
        description,
        stdin_is_terminal,
        &launch_editor,
        &std::env::temp_dir,
    )
}

/// Resolve one CLI region-flag value: `@path` reads (and trims) that file's
/// contents; anything else is literal text. Unlike `--task`, the `@` is an
/// explicit file marker, so a missing `@file` is an error (the user meant a
/// file), not a literal fallback.
pub fn read_region_value(raw: &str) -> anyhow::Result<String> {
    match raw.strip_prefix('@') {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read region file '{}': {}", path, e))?;
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("Region file '{}' is empty.", path);
            }
            Ok(trimmed)
        }
        None => Ok(raw.to_string()),
    }
}

/// Same as [`resolve_task_with`], but with the editor launch itself injected
/// too — lets tests deterministically exercise `launch_editor`'s error
/// propagating out of `resolve_task_with` (the `result?` a few lines down)
/// without needing a real failing subprocess/PATH setup. On Windows there is
/// no safe way to make the real `launch_editor`'s platform-default candidate
/// (`notepad`, resolved via `System32` unconditionally) fail without
/// mutating the real system directory, so `resolve_task_with`'s own
/// `#[cfg(unix)]`-only real-PATH-starvation test for this can't be mirrored
/// there — injecting the editor launcher closes that gap on every platform.
///
/// Also takes the temp-directory provider (`tmp_dir_fn`) as an injectable
/// closure so tests can point the task-template write at a guaranteed-
/// unwritable directory (e.g. one whose parent doesn't exist) and
/// deterministically exercise `write_task_template`'s `?` propagating out of
/// this function -- the real OS temp directory used in production is
/// essentially always writable, so that error path is otherwise untestable.
///
/// All closures are `&dyn Fn` for the same monomorphization-noise reason
/// documented on [`resolve_task_with`].
fn resolve_task_with_editor(
    arg: &Option<String>,
    agent_name: &str,
    description: Option<&str>,
    stdin_is_terminal: &dyn Fn() -> bool,
    launch_editor_fn: &dyn Fn(&std::path::Path) -> anyhow::Result<()>,
    tmp_dir_fn: &dyn Fn() -> std::path::PathBuf,
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
            let tmp_path = tmp_dir_fn().join(format!("lev-task-{}.txt", std::process::id()));
            write_task_template(&tmp_path, &template)?;

            // Launch the editor (exits only when the user closes it)
            let result = launch_editor_fn(&tmp_path);
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
    if let Some(desc) = description
        && !desc.is_empty()
    {
        template.push_str(&format!("# {}\n", desc));
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
/// Outcome of running one editor candidate, abstracting over the raw
/// `ExitStatus`. This exists so the "ran but ended with no exit code" case (a
/// signal kill on Unix) is injectable in tests on *every* platform: on Windows
/// an `ExitStatus` always carries a code (even via `ExitStatusExt::from_raw`),
/// so that case cannot be fabricated from a status directly. The injected `run`
/// seam of [`launch_editor_with`] therefore yields this enum rather than an
/// `ExitStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorRunOutcome {
    /// Process finished (success, or any explicit exit code) — treat as the
    /// user having closed the editor.
    Completed,
    /// Process ended with no exit code (e.g. killed by a signal) — try the next
    /// candidate.
    Aborted,
}

/// Classify an editor subprocess's exit. `code == None` means it ended without
/// an exit code (a signal kill). A pure function so both arms are unit-testable
/// on every platform, independent of whether a real process can produce a
/// code-less status there.
fn classify_editor_exit(success: bool, code: Option<i32>) -> EditorRunOutcome {
    if success || code.is_some() {
        EditorRunOutcome::Completed
    } else {
        EditorRunOutcome::Aborted
    }
}

fn launch_editor(path: &std::path::Path) -> anyhow::Result<()> {
    launch_editor_with(path, &mut |cmd| {
        cmd.status()
            .map(|s| classify_editor_exit(s.success(), s.code()))
    })
}

/// Core of [`launch_editor`], with the actual "run this candidate and get its
/// exit status" step injected as `run` instead of hardcoded to
/// `Command::status()`.
///
/// This seam exists specifically so the final "no editor found" `bail!` below
/// can be exercised deterministically on every platform. On Unix that branch
/// is reachable by starving `$PATH` so even the real `vim`/`nano`/`vi`
/// fallbacks fail to resolve (see the real-subprocess tests below), but there
/// is no safe real-subprocess equivalent on Windows: `Command::new("notepad")`
/// resolves via the `System32` search path that `CreateProcess` consults
/// *before* `$PATH`, so it can't be made to fail short of tampering with a
/// real system directory. Injecting `run` lets a single, platform-independent
/// test force every candidate to fail with `NotFound` without spawning any
/// process at all -- proving the `bail!` is reachable production code on
/// every platform, not a permanent gap.
///
/// `run` is `&mut dyn FnMut` rather than `impl FnMut` for the same
/// monomorphization-noise reason documented on
/// [`resolve_task_with`](super::session::resolve_task_with): several test
/// call sites below pass distinct closure literals directly to this
/// function, and a generic parameter would give each one its own
/// instantiation.
fn launch_editor_with(
    path: &std::path::Path,
    run: &mut dyn FnMut(&mut std::process::Command) -> std::io::Result<EditorRunOutcome>,
) -> anyhow::Result<()> {
    use std::process::Command;

    // Resolve editor candidates in priority order
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(v) = std::env::var("VISUAL")
        && !v.is_empty()
    {
        candidates.push(v);
    }
    if let Ok(e) = std::env::var("EDITOR")
        && !e.is_empty()
    {
        candidates.push(e);
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

        match run(&mut cmd) {
            // Exited (even non-zero means the user closed it — treat as OK).
            Ok(EditorRunOutcome::Completed) => {
                return Ok(());
            }
            Ok(EditorRunOutcome::Aborted) => {
                // Ended with no exit code (e.g. killed by signal on Unix) —
                // try the next candidate.
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

/// Build the list of [`ProviderCreds`] a [`Config`] implies. `ollama` is always
/// present (it needs no key); the API-key providers are included only when their
/// key is configured, and `claude-code` only when explicitly enabled. This is the
/// sole point that reads provider settings out of `Config`.
pub fn provider_creds_from_config(config: &Config) -> Vec<ProviderCreds> {
    let caps = &config.model_capabilities;
    let timeout = config.request_timeout_secs;
    let mut creds = Vec::new();

    let keyed = [
        ("anthropic", config.providers.anthropic_api_key.as_ref()),
        ("openai", config.providers.openai_api_key.as_ref()),
        ("google", config.providers.google_api_key.as_ref()),
        ("openrouter", config.openrouter_api_key.as_ref()),
    ];
    for (name, key) in keyed {
        if let Some(key) = key {
            creds.push(ProviderCreds {
                name: name.to_string(),
                api_key: Some(key.clone()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: timeout,
                options: std::collections::HashMap::new(),
            });
        }
    }

    // Ollama is always available (no key); carry any configured base URL.
    creds.push(ProviderCreds {
        name: "ollama".to_string(),
        api_key: None,
        base_url: Some(
            config
                .ollama_base_url
                .as_deref()
                .unwrap_or("http://localhost:11434")
                .to_string(),
        ),
        model_capabilities: caps.clone(),
        request_timeout_secs: timeout,
        options: std::collections::HashMap::new(),
    });

    // Claude Code needs no API key, but it is opt-in rather than always-on: the
    // CLI puts the user's account email address into every call and that cannot
    // be turned off. Leaving it unregistered is also how it stays out of an
    // agent's model fallback chain — `resolve_stage_model` skips any provider
    // the registry doesn't have.
    if config.providers.claude_code_enabled {
        let mut options = std::collections::HashMap::new();
        if let Some(binary) = &config.providers.claude_code_binary {
            options.insert("binary".to_string(), binary.clone());
        }
        if let Some(effort) = &config.providers.claude_code_effort {
            options.insert("effort".to_string(), effort.clone());
        }
        creds.push(ProviderCreds {
            name: "claude-code".to_string(),
            api_key: None,
            base_url: None,
            model_capabilities: caps.clone(),
            request_timeout_secs: None,
            options,
        });
    }

    creds
}

/// Convenience wrapper: build a [`ProviderRegistry`] straight from a [`Config`].
///
/// Kept as a `fn(&Config) -> ProviderRegistry` so it can be passed as the
/// registry-builder seam that `run`/`models`/`dashboard` inject for tests.
///
/// Native providers are registered eagerly from [`provider_creds_from_config`];
/// a [`ScriptProviderLayer`](leviath_runtime::script_provider::ScriptProviderLayer)
/// is then attached so Rhai *script providers* (issue #101) resolve lazily and
/// hot-reload from `~/.leviath/providers/`.
pub fn build_provider_registry_from_config(config: &Config) -> ProviderRegistry {
    let registry = build_provider_registry(&provider_creds_from_config(config));
    attach_script_layer(registry, crate::config::providers_dir(), config)
}

/// Attach a [`ScriptProviderLayer`](leviath_runtime::script_provider::ScriptProviderLayer)
/// over `dir` (the providers directory) when one is available; otherwise return
/// the registry unchanged. Split out so both the with-dir and no-home paths are
/// unit-testable.
fn attach_script_layer(
    registry: ProviderRegistry,
    dir: Option<std::path::PathBuf>,
    config: &Config,
) -> ProviderRegistry {
    let Some(dir) = dir else {
        return registry;
    };
    let overrides = config
        .model_providers
        .iter()
        .map(|(name, mp)| (name.clone(), script_provider_spec(mp)))
        .collect();
    let layer = leviath_runtime::script_provider::ScriptProviderLayer::new(
        dir,
        overrides,
        config.model_capabilities.clone(),
        config.request_timeout_secs,
        config.security.allow_env_vars.clone(),
    );
    registry.with_script_layer(std::sync::Arc::new(layer))
}

/// Translate a CLI [`ModelProviderConfig`](crate::config::ModelProviderConfig)
/// into the runtime's plain-data
/// [`ScriptProviderSpec`](leviath_runtime::script_provider::ScriptProviderSpec):
/// `base_url`/`api_key`/extra keys become the `initialize(config)` map.
fn script_provider_spec(
    mp: &crate::config::ModelProviderConfig,
) -> leviath_runtime::script_provider::ScriptProviderSpec {
    let mut cfg = serde_json::Map::new();
    if let Some(b) = &mp.base_url {
        cfg.insert("base_url".to_string(), serde_json::Value::String(b.clone()));
    }
    if let Some(k) = &mp.api_key {
        cfg.insert("api_key".to_string(), serde_json::Value::String(k.clone()));
    }
    for (k, v) in &mp.extra {
        cfg.insert(
            k.clone(),
            serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
        );
    }
    leviath_runtime::script_provider::ScriptProviderSpec {
        script: mp.script.clone(),
        rate_limit: mp.rate_limit.clone(),
        init_config: serde_json::Value::Object(cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared "stdin is never a TTY" probe for the `resolve_task` tests whose
    /// argument is `Some(..)` (so the probe is never consulted) or that
    /// explicitly want the non-TTY error path. A single named `fn` (rather than
    /// a fresh `|| false` closure per call site) keeps every call site sharing
    /// one instantiation and one covered region.
    fn never_a_tty() -> bool {
        false
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

    // ─── read_region_value ────────────────────────────────────────────────

    #[test]
    fn read_region_value_literal_passthrough() {
        assert_eq!(read_region_value("just text").unwrap(), "just text");
    }

    #[test]
    fn read_region_value_at_path_reads_and_trims() {
        let dir = std::env::temp_dir().join("lev-test-region-value");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("r.md");
        std::fs::write(&file, "  hello region  \n").unwrap();
        let raw = format!("@{}", file.to_string_lossy());
        assert_eq!(read_region_value(&raw).unwrap(), "hello region");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_region_value_at_missing_file_errors() {
        let err = read_region_value("@/no/such/region/file.md").unwrap_err();
        assert!(err.to_string().contains("Failed to read region file"));
    }

    #[test]
    fn read_region_value_at_empty_file_errors() {
        let dir = std::env::temp_dir().join("lev-test-region-empty");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("empty.md");
        std::fs::write(&file, "   \n").unwrap();
        let raw = format!("@{}", file.to_string_lossy());
        let err = read_region_value(&raw).unwrap_err();
        assert!(err.to_string().contains("is empty"));
        std::fs::remove_dir_all(&dir).ok();
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
        let result = resolve_task(
            &Some("do something".to_string()),
            "test",
            None,
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "do something");
    }

    #[test]
    fn resolve_task_with_file_path() {
        let dir = std::env::temp_dir().join("lev-test-resolve-task");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("task.txt");
        std::fs::write(&file, "task from file\n").unwrap();

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "test",
            None,
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "task from file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_task_with_empty_file_errors() {
        let dir = std::env::temp_dir().join("lev-test-resolve-empty");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("empty.txt");
        std::fs::write(&file, "   \n  ").unwrap();

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "test",
            None,
            &never_a_tty,
        );
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
            &never_a_tty,
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

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "test",
            None,
            &never_a_tty,
        );
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

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "test",
            None,
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "hello world");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_task_preserves_literal_string_as_is() {
        let result = resolve_task(
            &Some("  spaces around  ".to_string()),
            "test",
            None,
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "  spaces around  ");
    }

    #[test]
    fn build_provider_registry_with_empty_config() {
        let config = Config::default();
        let registry = build_provider_registry_from_config(&config);
        // Ollama needs no key and is always on.
        assert!(registry.has("ollama"));
        // Claude Code needs no key either, but is opt-in — a default config
        // must not reach the user's Claude subscription (or send their account
        // email to it) without them having said yes.
        assert!(!registry.has("claude-code"));
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
        let registry = build_provider_registry_from_config(&config);
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
        let registry = build_provider_registry_from_config(&config);
        assert!(registry.has("openai"));
    }

    #[test]
    fn build_provider_registry_with_google_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                google_api_key: Some("AIzatest12345".to_string()),
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry = build_provider_registry_from_config(&config);
        assert!(registry.has("google"));
    }

    #[test]
    fn build_provider_registry_with_openrouter_key() {
        let config = Config {
            openrouter_api_key: Some("sk-or-test-12345".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry_from_config(&config);
        assert!(registry.has("openrouter"));
    }

    #[test]
    fn build_provider_registry_custom_ollama_url() {
        let config = Config {
            ollama_base_url: Some("http://my-server:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry_from_config(&config);
        assert!(registry.has("ollama"));
    }

    #[test]
    fn script_provider_spec_assembles_init_config() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("region".to_string(), toml::Value::String("us".to_string()));
        let mp = crate::config::ModelProviderConfig {
            script: Some("groq".to_string()),
            api_key: Some("k".to_string()),
            base_url: Some("http://api".to_string()),
            rate_limit: Some(leviath_providers::RateLimitConfig {
                requests_per_minute: 30,
                tokens_per_minute: 1000,
            }),
            extra,
        };
        let spec = script_provider_spec(&mp);
        assert_eq!(spec.script.as_deref(), Some("groq"));
        assert!(spec.rate_limit.is_some());
        assert_eq!(spec.init_config["base_url"], "http://api");
        assert_eq!(spec.init_config["api_key"], "k");
        assert_eq!(spec.init_config["region"], "us");
    }

    #[test]
    fn attach_script_layer_without_home_is_a_noop() {
        // No providers directory (no resolvable home) → registry unchanged, no
        // script provider resolves.
        let registry = attach_script_layer(ProviderRegistry::new(), None, &Config::default());
        assert!(!registry.has("groq"));
    }

    #[test]
    fn build_registry_resolves_a_configured_script_provider() {
        let home = tempfile::tempdir().unwrap();
        let providers = home.path().join(".leviath").join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(
            providers.join("groq.rhai"),
            "fn initialize(config) { #{} }\nfn inference(state, request) { #{ content: \"ok\" } }",
        )
        .unwrap();

        let mut model_providers = std::collections::HashMap::new();
        model_providers.insert(
            "groq".to_string(),
            crate::config::ModelProviderConfig::default(),
        );
        let config = Config {
            model_providers,
            ..Config::default()
        };
        temp_env::with_var("LEVIATH_HOME", Some(home.path().as_os_str()), || {
            let registry = build_provider_registry_from_config(&config);
            assert!(registry.has("groq"));
            assert!(registry.get("groq").is_some());
        });
    }

    // ─── build_provider_registry with all keys ──────────────────────────

    #[test]
    fn build_provider_registry_all_keys_set() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: Some("sk-test".to_string()),
                google_api_key: Some("AIza-test".to_string()),
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            openrouter_api_key: Some("sk-or-test".to_string()),
            ollama_base_url: Some("http://custom:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry_from_config(&config);
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        // Every key in the world doesn't enable Claude Code — only opting in does.
        assert!(!registry.has("claude-code"));
    }

    // ─── ProviderCreds seam ─────────────────────────────────────────────

    #[test]
    fn provider_creds_from_config_includes_defaults_and_keyed() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant".to_string()),
                ..Config::default().providers
            },
            ollama_base_url: Some("http://custom:11434".to_string()),
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let names: Vec<&str> = creds.iter().map(|c| c.name.as_str()).collect();
        // anthropic (keyed) + ollama, but not openai/google/openrouter, and not
        // claude-code (opt-in, not enabled here).
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"ollama"));
        assert!(!names.contains(&"claude-code"));
        assert!(!names.contains(&"openai"));
        assert!(!names.contains(&"google"));
        assert!(!names.contains(&"openrouter"));
        // The ollama base URL is carried through.
        let ollama = creds.iter().find(|c| c.name == "ollama").unwrap();
        assert_eq!(ollama.base_url.as_deref(), Some("http://custom:11434"));
        assert!(ollama.api_key.is_none());
    }

    // ─── resolve_task: multiline file content ───────────────────────────

    #[test]
    fn resolve_task_multiline_file() {
        let dir = std::env::temp_dir().join("lev-test-resolve-multiline");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("multi.txt");
        std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "test",
            None,
            &never_a_tty,
        );
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
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "Write a function that does X & Y <html>");
    }

    // ─── build_provider_registry: no providers except defaults ──────────

    #[test]
    fn build_provider_registry_defaults_have_ollama_only() {
        let config = Config::default();
        let registry = build_provider_registry_from_config(&config);
        // Ollama is present regardless of key configuration; claude-code is not,
        // until the user opts in.
        assert!(registry.has("ollama"));
        assert!(!registry.has("claude-code"));
    }

    #[test]
    fn enabling_claude_code_registers_it_with_its_options() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                claude_code_enabled: true,
                claude_code_binary: Some("/opt/bin/claude".to_string()),
                claude_code_effort: Some("low".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let cc = creds
            .iter()
            .find(|c| c.name == "claude-code")
            .expect("enabled ⇒ present");
        assert_eq!(
            cc.options.get("binary").map(String::as_str),
            Some("/opt/bin/claude")
        );
        assert_eq!(cc.options.get("effort").map(String::as_str), Some("low"));
        assert!(cc.api_key.is_none());
        assert!(build_provider_registry_from_config(&config).has("claude-code"));
    }

    #[test]
    fn enabling_claude_code_without_options_carries_none() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                claude_code_enabled: true,
                ..Config::default().providers
            },
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let cc = creds.iter().find(|c| c.name == "claude-code").unwrap();
        // Absent settings stay absent so the provider applies its own defaults
        // (the `claude` binary on PATH, DEFAULT_EFFORT).
        assert!(cc.options.is_empty());
    }

    // ─── resolve_task: file with only comments in editor-like format ────

    #[test]
    fn resolve_task_literal_empty_string() {
        // Empty string is treated as literal, returns as-is
        let result = resolve_task(&Some("".to_string()), "test", None, &never_a_tty);
        assert_eq!(result.unwrap(), "");
    }

    // ─── resolve_task: file with multiple lines and trailing whitespace ──

    #[test]
    fn resolve_task_file_with_multiple_trailing_newlines() {
        let dir = std::env::temp_dir().join("lev-test-resolve-trail");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("trail.txt");
        std::fs::write(&file, "task content\n\n\n\n").unwrap();

        let result = resolve_task(
            &Some(file.to_str().unwrap().to_string()),
            "test",
            None,
            &never_a_tty,
        );
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            ..crate::config::Config::default()
        };
        let registry = build_provider_registry_from_config(&config);
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
            &never_a_tty,
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
        let registry = build_provider_registry_from_config(&config);
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
        let result = resolve_task_with(&None, "test-agent", None, &|| false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("No task provided"));
        assert!(msg.contains("stdin is not a TTY"));
    }

    #[test]
    fn resolve_task_none_arg_uses_injected_probe_via_public_wrapper() {
        // Smoke test that the public `resolve_task()` wrapper still compiles
        // and delegates to `resolve_task_with` with the caller-supplied probe.
        // A literal task never consults the probe, so the outcome is
        // deterministic regardless of environment.
        let result = resolve_task(
            &Some("literal task".to_string()),
            "test-agent",
            None,
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "literal task");
    }

    #[test]
    fn resolve_task_none_arg_errors_via_public_wrapper_when_not_a_tty() {
        // Drives `resolve_task`'s public wrapper with an injected "never a TTY"
        // probe: no task + not-a-TTY hits the "no task provided" error,
        // cross-platform, without touching real stdin or launching an editor.
        let result = resolve_task(&None, "test-agent", None, &never_a_tty);
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
        temp_env::with_vars(
            [("VISUAL", Some("/usr/bin/true")), ("EDITOR", None)],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-visual");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor: EDITOR used when VISUAL unset ────────────────────

    #[cfg(unix)]
    #[test]
    fn launch_editor_editor_env_success() {
        temp_env::with_vars(
            [("VISUAL", None), ("EDITOR", Some("/usr/bin/true"))],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-editor");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor: exit code (even non-zero) is treated as success ──

    #[cfg(unix)]
    #[test]
    fn launch_editor_nonzero_exit_still_ok() {
        temp_env::with_vars(
            [("VISUAL", Some("/usr/bin/false")), ("EDITOR", None)],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-nonzero");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                // A non-zero-but-present exit code is treated as the user having
                // closed the editor -- not an error.
                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor: terminated by signal (no exit code) tries next ───

    #[test]
    fn classify_editor_exit_success_is_completed() {
        assert_eq!(
            classify_editor_exit(true, Some(0)),
            EditorRunOutcome::Completed
        );
    }

    #[test]
    fn classify_editor_exit_nonzero_code_is_completed() {
        // Non-zero but present exit code = user closed the editor = done.
        assert_eq!(
            classify_editor_exit(false, Some(1)),
            EditorRunOutcome::Completed
        );
    }

    #[test]
    fn classify_editor_exit_no_code_is_aborted() {
        // No exit code (e.g. killed by a Unix signal) = try the next candidate.
        // Exercised here as a pure function so it's covered on every platform,
        // including Windows where a real `ExitStatus` always carries a code.
        assert_eq!(classify_editor_exit(false, None), EditorRunOutcome::Aborted);
    }

    #[test]
    fn launch_editor_with_aborted_candidate_falls_through_to_next() {
        // An injected `run` reporting `Aborted` exercises the `Ok(Aborted) => {}`
        // arm (try the next candidate) on every platform, without needing a real
        // signal-killed subprocess (which can't be fabricated on Windows). With
        // every candidate aborting, the loop exhausts them and bails.
        temp_env::with_vars(
            [("VISUAL", Some("editor-a")), ("EDITOR", Some("editor-b"))],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-aborted");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let mut calls = 0;
                let result = launch_editor_with(&file, &mut |_cmd| {
                    calls += 1;
                    Ok(EditorRunOutcome::Aborted)
                });
                // Every candidate "ran" but aborted, so it tried them all then bailed.
                assert!(result.is_err());
                assert!(calls >= 2, "expected multiple candidates to be tried");

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor: command with flags is split correctly ────────────

    #[cfg(unix)]
    #[test]
    fn launch_editor_command_with_flags_splits_correctly() {
        // `/usr/bin/true` ignores all arguments, so appending a flag
        // and the file path is harmless; this exercises the
        // whitespace-splitting logic for editor strings like
        // "code --wait".
        temp_env::with_vars(
            [
                ("VISUAL", Some("/usr/bin/true --some-flag")),
                ("EDITOR", None),
            ],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-flags");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor: whitespace-only VISUAL falls through, EDITOR used ─

    #[cfg(unix)]
    #[test]
    fn launch_editor_whitespace_only_visual_falls_through_to_editor() {
        // Whitespace-only string is non-empty so it IS pushed as a
        // candidate, but splitting on whitespace yields an empty parts
        // vec, which triggers the `continue` branch.
        temp_env::with_vars(
            [("VISUAL", Some("   ")), ("EDITOR", Some("/usr/bin/true"))],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-ws-visual");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor_with: truly-empty VISUAL/EDITOR, injected (cross-platform) ─

    /// Exercises the `!v.is_empty()`/`!e.is_empty()` false arm (empty-string
    /// `VISUAL`/`EDITOR` never gets pushed onto the candidates list) the same
    /// way the "no editor found" test above closes the Windows gap: via the
    /// injected `run` seam instead of PATH-starvation.
    ///
    /// The Unix test below needs PATH-starvation only to keep an
    /// empty-VISUAL/EDITOR fallthrough to the real `vim`/`nano`/`vi`
    /// platform defaults from actually launching a real, blocking editor --
    /// it isn't inherent to the branch itself. That real-editor risk doesn't
    /// exist here: `run` never spawns anything real regardless of which
    /// candidate string `launch_editor_with` resolved to, so this needs no
    /// PATH manipulation (and thus no `PATH_ENV_LOCK`) at all. Re-spawning
    /// the current test binary (rather than fabricating a `std::process::
    /// ExitStatus` directly, which has no portable stable constructor) gives
    /// a real, immediate, always-terminates `ExitStatus` on every platform --
    /// same technique `commands::serve::agents` uses to get a real child
    /// process without depending on what it actually does.
    #[test]
    fn launch_editor_with_empty_visual_and_editor_are_skipped() {
        temp_env::with_vars([("VISUAL", Some("")), ("EDITOR", Some(""))], || {
            let dir = std::env::temp_dir().join("lev-test-launch-editor-with-empty-skip");
            let _ = std::fs::create_dir_all(&dir);
            let file = dir.join("edit.txt");
            std::fs::write(&file, "content").unwrap();

            let result = launch_editor_with(&file, &mut |_cmd| {
                // Ignores the actual candidate `launch_editor_with` resolved to
                // (the platform default, since VISUAL/EDITOR are both empty) and
                // spawns the current test binary instead -- any exit status it
                // produces (even a nonzero "unrecognized option" error) classifies
                // as `Completed`.
                std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("--this-flag-does-not-exist")
                    .status()
                    .map(|s| classify_editor_exit(s.success(), s.code()))
            });
            assert_launch_ok(&result);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    // ─── launch_editor: truly-empty VISUAL/EDITOR are skipped entirely ────

    // Unlike the whitespace-only case above (non-empty string, pushed as a
    // candidate that then fails to split into any usable parts), a truly
    // empty `VISUAL`/`EDITOR` value never even gets pushed onto the
    // candidates list -- exercising the `!v.is_empty()`/`!e.is_empty()`
    // false arm for both. With both vars empty, resolution falls through to
    // the unix platform defaults (vim/nano/vi) unless PATH is also starved --
    // so this test combines the empty-string case with the same
    // PATH-starvation trick as `launch_editor_no_editor_found_when_path_has_no_candidates`
    // below, guaranteeing a deterministic `Err` instead of ever risking a
    // real, blocking, interactive editor launch. Kept as extra real-PATH
    // insurance on Unix alongside the injected-seam version above, which is
    // what actually closes the Windows gap for this branch.
    #[cfg(unix)]
    #[test]
    fn launch_editor_empty_visual_and_editor_are_skipped() {
        temp_env::with_vars(
            [
                ("VISUAL", Some("")),
                ("EDITOR", Some("")),
                ("PATH", Some("/lev-definitely-empty-path-dir")),
            ],
            || {
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
            },
        );
    }

    // ─── launch_editor: NotFound candidate is skipped, next one used ─────

    #[cfg(unix)]
    #[test]
    fn launch_editor_not_found_candidate_falls_through_to_next() {
        // VISUAL points at a nonexistent binary, which should be
        // skipped (NotFound branch, `continue`) in favor of EDITOR.
        temp_env::with_vars(
            [
                ("VISUAL", Some("lev-definitely-not-a-real-binary-xyz")),
                ("EDITOR", Some("/usr/bin/true")),
            ],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-notfound");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor: non-NotFound spawn error propagates ──────────────

    #[cfg(unix)]
    #[test]
    fn launch_editor_permission_denied_returns_error() {
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

        temp_env::with_vars(
            [("VISUAL", Some(&not_executable)), ("EDITOR", None)],
            || {
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert!(result.is_err());
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Failed to launch editor")
                );

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── launch_editor_with: no candidate resolves (injected, cross-platform) ─

    /// Exercises the final `bail!("No editor found...")` in
    /// [`launch_editor_with`] via the injected `run` seam rather than real
    /// PATH/filesystem state -- see the doc comment on `launch_editor_with`
    /// for why that matters on Windows specifically (real PATH-starvation
    /// can't fail `Command::new("notepad")`, which resolves via `System32`
    /// unconditionally). Forcing every candidate to fail with `NotFound`
    /// here doesn't depend on the platform at all: no real process is ever
    /// spawned, so this runs identically -- and actually proves the `bail!`
    /// line is reachable production code -- on Unix, Windows, and macOS
    /// alike. Doesn't need `ENV_LOCK`/`PATH_ENV_LOCK`: whatever `$VISUAL`/
    /// `$EDITOR` happen to be set to by a concurrently-running test is
    /// irrelevant, since the injected closure fails every candidate the same
    /// way regardless of its name.
    #[test]
    fn launch_editor_with_no_editor_found_when_every_candidate_not_found() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-with-no-editor");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("edit.txt");
        std::fs::write(&file, "content").unwrap();

        let result = launch_editor_with(&file, &mut |_cmd| {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No editor found"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── launch_editor: no candidate resolves anywhere on PATH ────────────

    // Windows resolves "notepad" via the System32 search path regardless of
    // $PATH, so PATH-starvation can't produce a "no editor found" outcome
    // there the way it does on Unix (breaking PATH so vim/nano/vi can't
    // resolve) -- gated to `unix` for the same real-blocking-editor-hang
    // reason as the tests above. Kept alongside
    // `launch_editor_with_no_editor_found_when_every_candidate_not_found`
    // above as extra real-subprocess insurance on Unix; the injected-seam
    // test is what actually closes the Windows gap.
    #[cfg(unix)]
    #[test]
    fn launch_editor_no_editor_found_when_path_has_no_candidates() {
        // No VISUAL/EDITOR (both unset), and PATH points nowhere -- so even
        // the unix platform-default candidates (vim/nano/vi) all fail to
        // resolve.
        temp_env::with_vars(
            [
                ("VISUAL", None),
                ("EDITOR", None),
                ("PATH", Some("/lev-definitely-empty-path-dir")),
            ],
            || {
                let dir = std::env::temp_dir().join("lev-test-launch-editor-no-editor");
                let _ = std::fs::create_dir_all(&dir);
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("No editor found"));

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
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
    // The "editor ended with no exit code" case (killed by a Unix signal) is a
    // Windows testability challenge: on Windows `ExitStatus::code()` is always
    // `Some(_)` (even via `ExitStatusExt::from_raw`), so no real or fabricated
    // status reaches the "try next candidate" arm there. The injected `run`
    // seam returns `EditorRunOutcome` rather than `ExitStatus`: the
    // status-to-outcome decision lives in the pure `classify_editor_exit`
    // (unit-tested for the code-less case on every platform), and the
    // "outcome == Aborted, try next" arm is driven directly via injection in
    // `launch_editor_with_aborted_candidate_falls_through_to_next` -- both
    // cross-platform, no code-less `ExitStatus` required.
    //
    // Three other Unix tests rely on PATH-starvation --
    // `launch_editor_empty_visual_and_editor_are_skipped`,
    // `launch_editor_no_editor_found_when_path_has_no_candidates`, and
    // `resolve_task_with_editor_path_propagates_launch_editor_error`.
    // PATH-starvation has no safe Windows equivalent: `Command::new("notepad")`
    // resolves via `System32` unconditionally before consulting `$PATH`, so
    // PATH-starvation can't make it fail there. Instead, injecting the "run
    // this candidate" step itself (`launch_editor_with`'s `run` parameter) or
    // the "launch the editor" step (`resolve_task_with_editor`'s
    // `launch_editor_fn` parameter) sidesteps real process resolution
    // entirely, closing all three gaps on every platform -- see
    // `launch_editor_with_no_editor_found_when_every_candidate_not_found`,
    // `launch_editor_with_empty_visual_and_editor_are_skipped`, and
    // `resolve_task_with_editor_injected_editor_failure_propagates` above.

    #[cfg(windows)]
    fn write_bat(path: &std::path::Path, body: &str) {
        std::fs::write(path, format!("@echo off\r\n{}\r\n", body)).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_visual_env_success() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-visual-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        temp_env::with_vars([("VISUAL", Some(&ok_bat)), ("EDITOR", None)], || {
            let file = dir.join("edit.txt");
            std::fs::write(&file, "content").unwrap();

            let result = launch_editor(&file);
            assert_launch_ok(&result);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_editor_env_success() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-editor-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        temp_env::with_vars([("VISUAL", None), ("EDITOR", Some(&ok_bat))], || {
            let file = dir.join("edit.txt");
            std::fs::write(&file, "content").unwrap();

            let result = launch_editor(&file);
            assert_launch_ok(&result);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_nonzero_exit_still_ok() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-nonzero-win");
        let _ = std::fs::create_dir_all(&dir);
        let fail_bat = dir.join("fail.bat");
        write_bat(&fail_bat, "exit /b 1");

        temp_env::with_vars([("VISUAL", Some(&fail_bat)), ("EDITOR", None)], || {
            let file = dir.join("edit.txt");
            std::fs::write(&file, "content").unwrap();

            // A non-zero-but-present exit code is treated as the user having
            // closed the editor -- not an error.
            let result = launch_editor(&file);
            assert_launch_ok(&result);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_command_with_flags_splits_correctly() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-flags-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        // The batch file ignores all arguments, so appending a flag and
        // the file path is harmless; this exercises the
        // whitespace-splitting logic for editor strings like
        // "code --wait".
        temp_env::with_vars(
            [
                ("VISUAL", Some(format!("{} --some-flag", ok_bat.display()))),
                ("EDITOR", None),
            ],
            || {
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_whitespace_only_visual_falls_through_to_editor() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-ws-visual-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        // Whitespace-only string is non-empty so it IS pushed as a
        // candidate, but splitting on whitespace yields an empty parts
        // vec, which triggers the `continue` branch.
        temp_env::with_vars(
            [
                ("VISUAL", Some(std::ffi::OsString::from("   "))),
                ("EDITOR", Some(ok_bat.clone().into_os_string())),
            ],
            || {
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_not_found_candidate_falls_through_to_next() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-notfound-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        // VISUAL points at a nonexistent binary, which should be
        // skipped (NotFound branch, `continue`) in favor of EDITOR.
        temp_env::with_vars(
            [
                (
                    "VISUAL",
                    Some(std::ffi::OsString::from(
                        "lev-definitely-not-a-real-binary-xyz",
                    )),
                ),
                ("EDITOR", Some(ok_bat.clone().into_os_string())),
            ],
            || {
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert_launch_ok(&result);

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    #[cfg(windows)]
    #[test]
    fn launch_editor_permission_denied_returns_error() {
        let dir = std::env::temp_dir().join("lev-test-launch-editor-perm-denied-win");
        let _ = std::fs::create_dir_all(&dir);
        // A plain, non-executable text file: Windows' `CreateProcess` can't
        // recognize it as an executable image and fails with
        // `ERROR_BAD_EXE_FORMAT` (os error 193), not `NotFound` -- exercising
        // the generic `Err(e)` arm (as opposed to the `NotFound` "try next
        // candidate" arm already covered above).
        let not_executable = dir.join("not-executable.txt");
        std::fs::write(&not_executable, "not a script").unwrap();

        temp_env::with_vars(
            [("VISUAL", Some(&not_executable)), ("EDITOR", None)],
            || {
                let file = dir.join("edit.txt");
                std::fs::write(&file, "content").unwrap();

                let result = launch_editor(&file);
                assert!(result.is_err());
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Failed to launch editor")
                );

                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }

    // ─── resolve_task_with: editor path (stdin is a TTY) ──────────────────

    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_happy_case() {
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

        temp_env::with_vars([("VISUAL", Some(&script)), ("EDITOR", None)], || {
            let result = resolve_task_with(
                &None,
                "test-agent",
                Some("a non-empty description"),
                &|| true,
            );
            assert_eq!(result.unwrap(), "task body from editor");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_empty_after_stripping_comments_errors() {
        // /usr/bin/true "opens" the file and does nothing to it, so only the
        // commented-out template remains -- stripped down to an empty task.
        temp_env::with_vars(
            [("VISUAL", Some("/usr/bin/true")), ("EDITOR", None)],
            || {
                let result = resolve_task_with(&None, "test-agent", None, &|| true);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("Aborting run"));
            },
        );
    }

    // Same Windows PATH-starvation caveat as
    // `launch_editor_no_editor_found_when_path_has_no_candidates` above. Kept
    // as extra real-PATH insurance on Unix alongside the injected-seam
    // version below, which is what actually closes the Windows gap.
    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_propagates_launch_editor_error() {
        // No VISUAL/EDITOR (both unset), and PATH points nowhere -- so even
        // the unix platform-default candidates (vim/nano/vi) all fail to
        // resolve, propagating the "no editor found" error.
        temp_env::with_vars(
            [
                ("VISUAL", None),
                ("EDITOR", None),
                ("PATH", Some("/lev-definitely-empty-path-dir")),
            ],
            || {
                let result = resolve_task_with(&None, "test-agent", None, &|| true);
                assert!(result.is_err());
                assert!(result.unwrap_err().to_string().contains("No editor found"));
            },
        );
    }

    /// Shared stub editor-launcher used by both
    /// `resolve_task_with_editor_injected_editor_failure_propagates` (where
    /// it's actually invoked) and
    /// `resolve_task_with_editor_tmp_file_write_failure_propagates` (where,
    /// by design, `write_task_template`'s earlier `?` should short-circuit
    /// before this is ever reached). Extracted into a single named `fn`
    /// rather than an inline closure per call site so that if the latter
    /// test's control flow ever regresses and this stub *does* get called,
    /// llvm-cov's function-level coverage for it is still merged from the
    /// former test -- an inline closure unique to the latter test would
    /// otherwise show up as a brand new "0 calls" function purely because
    /// that particular test is designed to never reach it.
    fn stub_editor_returns_no_editor_found(_path: &std::path::Path) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "No editor found. Set $VISUAL or $EDITOR, or install vim/nano/notepad."
        ))
    }

    /// Cross-platform twin of
    /// `resolve_task_with_editor_path_propagates_launch_editor_error` via
    /// `resolve_task_with_editor`'s injected editor launcher -- see that
    /// function's doc comment for why real PATH-starvation can't be mirrored
    /// on Windows here. Doesn't touch `PATH`/`VISUAL`/`EDITOR` at all (no
    /// `ENV_LOCK`/`PATH_ENV_LOCK` needed): the injected closure fails
    /// unconditionally regardless of environment state.
    #[test]
    fn resolve_task_with_editor_injected_editor_failure_propagates() {
        let result = resolve_task_with_editor(
            &None,
            "test-agent",
            None,
            &|| true,
            &stub_editor_returns_no_editor_found,
            &std::env::temp_dir,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No editor found"));
    }

    /// Exercises `write_task_template(&tmp_path, &template)?`'s error path at
    /// its actual call site inside `resolve_task_with_editor` (as opposed to
    /// `write_task_template_error_on_bad_path` below, which calls
    /// `write_task_template` directly). The real OS temp directory used in
    /// production is essentially always writable, so this is only reachable
    /// at all via the injected `tmp_dir_fn` -- pointed here at a directory
    /// whose parent doesn't exist, so the write fails deterministically on
    /// both Unix (ENOENT) and Windows (ERROR_PATH_NOT_FOUND) before the
    /// editor launcher is ever reached (if it *were* reached, the assertion
    /// below on the error message would fail, since
    /// `stub_editor_returns_no_editor_found`'s error text differs).
    #[test]
    fn resolve_task_with_editor_tmp_file_write_failure_propagates() {
        let bad_tmp_dir = std::env::temp_dir()
            .join("lev-definitely-nonexistent-parent-dir-for-task-template-xyz")
            .join("nested");
        let result = resolve_task_with_editor(
            &None,
            "test-agent",
            None,
            &|| true,
            &stub_editor_returns_no_editor_found,
            &move || bad_tmp_dir.clone(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to create task temp file")
        );
    }

    // ─── resolve_task_with: editor path (stdin is a TTY) — Windows twins ──

    #[cfg(windows)]
    #[test]
    fn resolve_task_with_editor_path_happy_case() {
        // A tiny batch "editor" that appends a non-comment line to whatever
        // file it's invoked on (%~1) -- standing in for a real interactive
        // editor session. `%~1` strips any surrounding quotes Windows adds
        // around a path containing spaces.
        let dir = std::env::temp_dir().join("lev-test-resolve-task-editor-happy-win");
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("fake-editor.bat");
        write_bat(&script, "echo task body from editor>>\"%~1\"");

        temp_env::with_vars([("VISUAL", Some(&script)), ("EDITOR", None)], || {
            let result = resolve_task_with(
                &None,
                "test-agent",
                Some("a non-empty description"),
                &|| true,
            );
            assert_eq!(result.unwrap(), "task body from editor");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[cfg(windows)]
    #[test]
    fn resolve_task_with_editor_path_empty_after_stripping_comments_errors() {
        // A no-op batch file "opens" the file and does nothing to it, so
        // only the commented-out template remains -- stripped down to an
        // empty task.
        let dir = std::env::temp_dir().join("lev-test-resolve-task-editor-empty-win");
        let _ = std::fs::create_dir_all(&dir);
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        temp_env::with_vars([("VISUAL", Some(&ok_bat)), ("EDITOR", None)], || {
            let result = resolve_task_with(&None, "test-agent", None, &|| true);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Aborting run"));

            let _ = std::fs::remove_dir_all(&dir);
        });
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to create task temp file")
        );
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
            &|| false,
        );
        // Restore perms before asserting (so cleanup works)
        let mut perms2 = std::fs::metadata(&file).unwrap().permissions();
        perms2.set_mode(0o644);
        std::fs::set_permissions(&file, perms2).ok();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read task file")
        );
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
            &|| false,
        );

        drop(_locked);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read task file")
        );
    }
}
