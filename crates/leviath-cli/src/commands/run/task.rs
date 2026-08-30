//! Turning what the caller typed into the text an agent actually runs on.
//!
//! Two inputs land here: `--task`, which is either a prompt or the path of a
//! file holding one, and the dynamic `--<region>` flags, whose values take an
//! explicit `@` before a path. Left off entirely, `--task` opens the user's
//! editor on a commented template.
//!
//! Everything in this module is Leviath policy rather than OS behavior, and
//! none of it carries a `#[cfg]`. Finding and running the editor itself is the
//! OS's business and lives in [`leviath_sys::editor`].

/// Resolve the task text from what `--task` was given, if anything.
///
/// - `Some(s)` naming an existing file → its contents, trimmed.
/// - `Some(s)` naming an existing directory → error.
/// - `Some(s)` naming nothing, but shaped like a path (no whitespace, and a
///   separator or a `~` prefix) → error, so a mistyped filename does not
///   quietly become the prompt.
/// - `Some(s)` otherwise → `s` as a literal prompt.
/// - `None` when stdin is not a TTY → error.
/// - `None` when stdin is a TTY → the user's editor on a commented template.
///
/// `description` comes from the blueprint and is `""` when the manifest sets
/// none; it only ever reaches the editor template.
///
/// `stdin_is_terminal` is injected (a `&dyn Fn() -> bool`) rather than probing
/// the real process stdin here, so the library core stays free of direct
/// `std::io::stdin()` access and is fully testable. In production the binary
/// passes `&|| std::io::stdin().is_terminal()`.
pub(crate) fn resolve_task(
    arg: Option<&str>,
    agent_name: &str,
    description: &str,
    stdin_is_terminal: &dyn Fn() -> bool,
) -> anyhow::Result<String> {
    resolve_task_with(arg, agent_name, description, stdin_is_terminal)
}

/// Whether `s` reads as a filesystem path rather than prompt text: no
/// whitespace anywhere, and either a path separator or a `~` home prefix.
///
/// This exists so a mistyped filename fails instead of silently becoming the
/// agent's entire task, which is what `lev run coder -t ./promt.md` used to do:
/// a real run, real tokens, and a transcript whose only instruction was an
/// eleven-character string. Prompt text that happens to mention a path ("fix
/// src/main.rs") always has spaces, so it never trips this.
///
/// `\` counts on every platform, not just Windows. Keeping the rule uniform
/// keeps the function pure and testable everywhere, and a whitespace-free
/// literal prompt containing a backslash is not a real prompt.
fn looks_like_path(s: &str) -> bool {
    !s.is_empty()
        && !s.chars().any(char::is_whitespace)
        && (s.contains('/') || s.contains('\\') || s.starts_with('~'))
}

/// Same as [`resolve_task`], but with the stdin-is-a-TTY check injected
/// instead of hardcoded - lets tests deterministically exercise both the
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
/// instantiations covers every source position - a confirmed llvm-cov
/// limitation (see `xtask/src/coverage.rs`'s doc comment on generic-function
/// monomorphization). Erasing the closure type with `&dyn Fn` collapses
/// every call site back down to a single instantiation, avoiding that noise
/// entirely.
fn resolve_task_with(
    arg: Option<&str>,
    agent_name: &str,
    description: &str,
    stdin_is_terminal: &dyn Fn() -> bool,
) -> anyhow::Result<String> {
    resolve_task_with_editor(
        arg,
        agent_name,
        description,
        stdin_is_terminal,
        &leviath_sys::editor::launch,
        &std::env::temp_dir,
    )
}

/// Resolve one CLI region-flag value: `@path` reads (and trims) that file's
/// contents; anything else is literal text. Unlike `--task`, the `@` is an
/// explicit file marker, so a missing `@file` is an error (the user meant a
/// file), not a literal fallback.
pub(crate) fn read_region_value(raw: &str) -> anyhow::Result<String> {
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
/// too - lets tests deterministically exercise the editor's error propagating
/// out of `resolve_task_with` (the `result?` a few lines down) without needing
/// a real failing subprocess or PATH setup. On Windows there is no safe way to
/// make the real launcher's platform default fail (`notepad` resolves via
/// `System32` unconditionally) short of mutating a real system directory, so
/// injecting the launcher is what closes that gap on every platform.
///
/// Also takes the temp-directory provider (`tmp_dir_fn`) as an injectable
/// closure so tests can point the task-template write at a guaranteed-
/// unwritable directory (e.g. one whose parent doesn't exist) and
/// deterministically exercise `write_task_template`'s `?` propagating out of
/// this function - the real OS temp directory used in production is
/// essentially always writable, so that error path is otherwise untestable.
///
/// All closures are `&dyn Fn` for the same monomorphization-noise reason
/// documented on [`resolve_task_with`].
fn resolve_task_with_editor(
    arg: Option<&str>,
    agent_name: &str,
    description: &str,
    stdin_is_terminal: &dyn Fn() -> bool,
    launch_editor_fn: &dyn Fn(&std::path::Path) -> std::io::Result<()>,
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
            // A directory is unambiguously an attempt to name a file, so say so
            // rather than sending the path itself to the agent as the prompt.
            if p.is_dir() {
                anyhow::bail!("Task file '{}' is a directory.", s);
            }
            if looks_like_path(s) {
                anyhow::bail!(
                    "No task file '{}'. Pass the prompt itself, or point --task at a file that exists.",
                    s
                );
            }
            Ok(s.to_string())
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

            // A randomly named file created `O_EXCL`, not `lev-task-<pid>.txt`.
            // A predictable name is an attack surface because `fs::write`
            // follows symlinks: on a shared host another user pre-creates that
            // path as a link to `~/.leviath/config.toml` or
            // `~/.ssh/authorized_keys`, and the next `lev run` writes the
            // template - and then everything the user types into their editor -
            // straight through it. `tempfile`
            // also creates it owner-only, so the task prompt is not world
            // readable while the editor holds it open.
            let tmp = write_task_template(&tmp_dir_fn(), &template)?;
            // Close our own handle before the editor opens the file: Windows
            // refuses a second writer while the first still holds it, so the
            // editor could not save. `TempPath` keeps the delete-on-drop.
            let tmp = tmp.into_temp_path();
            let tmp_path = tmp.to_path_buf();

            // Launch the editor (exits only when the user closes it)
            let result = launch_editor_fn(&tmp_path);
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp_path);
            // The launcher speaks `io::Error` because it lives in `leviath-sys`,
            // which carries no `anyhow`. Its messages are already user-facing.
            result.map_err(|e| anyhow::anyhow!("{e}"))?;

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

fn build_task_template(agent_name: &str, description: &str) -> String {
    let mut template = format!("# Task for agent: {}\n", agent_name);
    if !description.is_empty() {
        template.push_str(&format!("# {}\n", description));
    }
    template.push_str("#\n# Describe your task below. Lines starting with '#' are ignored.\n\n");
    template
}

fn write_task_template(
    dir: &std::path::Path,
    content: &str,
) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write as _;

    // Creating and writing in one fallible step, through the handle the builder
    // opened. Two steps would mean re-opening by path between them - a window in
    // which the name could be swapped - and a second error arm that a freshly
    // created, writable handle can never actually take.
    // A combinator chain rather than `?`s: each `?` would be an error arm that
    // a freshly created, writable handle can never take, and the whole point of
    // reporting here is the one failure that is real - the file could not be
    // created at all.
    tempfile::Builder::new()
        .prefix("lev-task-")
        .suffix(".txt")
        .tempfile_in(dir)
        .and_then(|mut file| {
            file.as_file_mut()
                .write_all(content.as_bytes())
                .and_then(|()| file.as_file_mut().flush())
                .map(|()| file)
        })
        .map_err(|e| anyhow::anyhow!("Failed to create task temp file: {}", e))
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
    // ─── read_region_value ────────────────────────────────────────────────

    #[test]
    fn read_region_value_literal_passthrough() {
        assert_eq!(read_region_value("just text").unwrap(), "just text");
    }

    #[test]
    fn read_region_value_at_path_reads_and_trims() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("r.md");
        std::fs::write(&file, "  hello region  \n").unwrap();
        let raw = format!("@{}", file.to_string_lossy());
        assert_eq!(read_region_value(&raw).unwrap(), "hello region");
    }

    #[test]
    fn read_region_value_at_missing_file_errors() {
        let err = read_region_value("@/no/such/region/file.md").unwrap_err();
        assert!(err.to_string().contains("Failed to read region file"));
    }

    #[test]
    fn read_region_value_at_empty_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("empty.md");
        std::fs::write(&file, "   \n").unwrap();
        let raw = format!("@{}", file.to_string_lossy());
        let err = read_region_value(&raw).unwrap_err();
        assert!(err.to_string().contains("is empty"));
    }

    #[test]
    fn resolve_task_with_literal_string() {
        let result = resolve_task(Some("do something"), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "do something");
    }

    #[test]
    fn resolve_task_with_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("task.txt");
        std::fs::write(&file, "task from file\n").unwrap();

        let result = resolve_task(Some(file.to_str().unwrap()), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "task from file");
    }

    #[test]
    fn resolve_task_with_empty_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("empty.txt");
        std::fs::write(&file, "   \n  ").unwrap();

        let result = resolve_task(Some(file.to_str().unwrap()), "test", "", &never_a_tty);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    /// A bare word that names no file is prompt text. It has no separator, so
    /// nobody could have meant it as a path.
    #[test]
    fn resolve_task_bare_word_that_is_not_a_file_is_literal() {
        let result = resolve_task(Some("do_something"), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "do_something");
    }

    /// The mistyped-filename case. Before this guard the agent was spawned with
    /// the path itself as its entire task.
    #[test]
    fn resolve_task_path_shaped_value_that_names_nothing_errors() {
        let err = resolve_task(
            Some("/nonexistent/path/do_something"),
            "test",
            "",
            &never_a_tty,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("No task file '/nonexistent/path/do_something'."),
            "{err}"
        );
    }

    /// A directory is unmistakably an attempt to name a file.
    #[test]
    fn resolve_task_directory_argument_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            resolve_task(Some(dir.path().to_str().unwrap()), "test", "", &never_a_tty).unwrap_err();
        assert!(err.to_string().ends_with("is a directory."), "{err}");
    }

    #[test]
    fn looks_like_path_accepts_separators_and_a_home_prefix() {
        assert!(looks_like_path("./a"));
        assert!(looks_like_path("a/b"));
        // Backslashes count on every platform, so the rule stays uniform.
        assert!(looks_like_path("a\\b"));
        assert!(looks_like_path("~/tasks/x.md"));
    }

    #[test]
    fn looks_like_path_rejects_prose_and_bare_words() {
        // Real prompts that mention a path always have spaces around it.
        assert!(!looks_like_path("fix src/main.rs"));
        // No separator at all.
        assert!(!looks_like_path("refactor"));
        assert!(!looks_like_path(""));
    }

    #[test]
    fn resolve_task_file_with_whitespace_only_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("whitespace.txt");
        std::fs::write(&file, "   \n\t\n  \n").unwrap();

        let result = resolve_task(Some(file.to_str().unwrap()), "test", "", &never_a_tty);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn resolve_task_file_trims_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("trimme.txt");
        std::fs::write(&file, "  hello world  \n\n").unwrap();

        let result = resolve_task(Some(file.to_str().unwrap()), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "hello world");
    }

    #[test]
    fn resolve_task_preserves_literal_string_as_is() {
        let result = resolve_task(Some("  spaces around  "), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "  spaces around  ");
    }

    #[test]
    fn resolve_task_multiline_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("multi.txt");
        std::fs::write(&file, "line one\nline two\nline three\n").unwrap();

        let result = resolve_task(Some(file.to_str().unwrap()), "test", "", &never_a_tty);
        let task = result.unwrap();
        assert!(task.contains("line one"));
        assert!(task.contains("line two"));
        assert!(task.contains("line three"));
    }

    // ─── resolve_task: literal string with special chars ────────────────

    #[test]
    fn resolve_task_literal_with_special_chars() {
        let result = resolve_task(
            Some("Write a function that does X & Y <html>"),
            "test",
            "",
            &never_a_tty,
        );
        assert_eq!(result.unwrap(), "Write a function that does X & Y <html>");
    }

    // ─── build_provider_registry: no providers except defaults ──────────

    #[test]
    fn resolve_task_literal_empty_string() {
        // Empty string is treated as literal, returns as-is
        let result = resolve_task(Some(""), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "");
    }

    // ─── resolve_task: file with multiple lines and trailing whitespace ──

    #[test]
    fn resolve_task_file_with_multiple_trailing_newlines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("trail.txt");
        std::fs::write(&file, "task content\n\n\n\n").unwrap();

        let result = resolve_task(Some(file.to_str().unwrap()), "test", "", &never_a_tty);
        assert_eq!(result.unwrap(), "task content");
    }

    // ─── build_provider_registry: model_capabilities propagated ──────────

    #[test]
    fn resolve_task_file_with_real_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("real.txt");
        std::fs::write(&file, "Implement a REST API server\nwith authentication\n").unwrap();

        let result = resolve_task(
            Some(file.to_str().unwrap()),
            "api-agent",
            "API agent",
            &never_a_tty,
        );
        let task = result.unwrap();
        assert!(task.contains("Implement a REST API server"));
        assert!(task.contains("with authentication"));
    }

    // ─── build_provider_registry: all providers registered ───────────────

    #[test]
    fn resolve_task_none_arg_errors_when_stdin_not_tty() {
        // The TTY check is injected (not the real std::io::stdin()) so this
        // is deterministic regardless of whether the test runner's own
        // stdin happens to be a real terminal - a human running `cargo test`
        // interactively has a real TTY on stdin, unlike CI, so hardcoding
        // "stdin is never a TTY under cargo test" was a false assumption.
        let result = resolve_task_with(None, "test-agent", "", &|| false);
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
        let result = resolve_task(Some("literal task"), "test-agent", "", &never_a_tty);
        assert_eq!(result.unwrap(), "literal task");
    }

    #[test]
    fn resolve_task_none_arg_errors_via_public_wrapper_when_not_a_tty() {
        // Drives `resolve_task`'s public wrapper with an injected "never a TTY"
        // probe: no task + not-a-TTY hits the "no task provided" error,
        // cross-platform, without touching real stdin or launching an editor.
        let result = resolve_task(None, "test-agent", "", &never_a_tty);
        assert!(result.is_err());
    }

    // ─── launch_editor: VISUAL takes priority and succeeds ───────────────

    // These `launch_editor` tests point VISUAL/EDITOR at `/usr/bin/true` (or
    // rely on PATH-starvation to prevent any editor being found) - both
    // assumptions are Unix-only. On Windows, `/usr/bin/true` doesn't exist,
    // so the NotFound-branch falls through to the windows-only "notepad"
    // candidate, which Windows resolves via its System32 search path
    // *regardless* of $PATH - so PATH-starvation doesn't stop it either.
    // Either way that means launching a real, blocking GUI text editor with
    // no timeout, which hung a Windows CI run indefinitely. Gated to `unix`.
    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_happy_case() {
        use std::os::unix::fs::PermissionsExt;

        // A tiny "editor" script that appends a non-comment line to
        // whatever file it's invoked on ($1) - standing in for a real
        // interactive editor session.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
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
            let result = resolve_task_with(None, "test-agent", "a non-empty description", &|| true);
            assert_eq!(result.unwrap(), "task body from editor");
        });
    }

    #[cfg(unix)]
    #[test]
    fn resolve_task_with_editor_path_empty_after_stripping_comments_errors() {
        // /usr/bin/true "opens" the file and does nothing to it, so only the
        // commented-out template remains - stripped down to an empty task.
        temp_env::with_vars(
            [("VISUAL", Some("/usr/bin/true")), ("EDITOR", None)],
            || {
                let result = resolve_task_with(None, "test-agent", "", &|| true);
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
        // No VISUAL/EDITOR (both unset), and PATH points nowhere - so even
        // the unix platform-default candidates (vim/nano/vi) all fail to
        // resolve, propagating the "no editor found" error.
        temp_env::with_vars(
            [
                ("VISUAL", None),
                ("EDITOR", None),
                ("PATH", Some("/lev-definitely-empty-path-dir")),
            ],
            || {
                let result = resolve_task_with(None, "test-agent", "", &|| true);
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
    /// former test - an inline closure unique to the latter test would
    /// otherwise show up as a brand new "0 calls" function purely because
    /// that particular test is designed to never reach it.
    fn stub_editor_returns_no_editor_found(_path: &std::path::Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No editor found. Set $VISUAL or $EDITOR, or install vim, nano, or edit.",
        ))
    }

    /// Cross-platform twin of
    /// `resolve_task_with_editor_path_propagates_launch_editor_error` via
    /// `resolve_task_with_editor`'s injected editor launcher - see that
    /// function's doc comment for why real PATH-starvation can't be mirrored
    /// on Windows here. Doesn't touch `PATH`/`VISUAL`/`EDITOR` at all (no
    /// `ENV_LOCK`/`PATH_ENV_LOCK` needed): the injected closure fails
    /// unconditionally regardless of environment state.
    #[test]
    fn resolve_task_with_editor_injected_editor_failure_propagates() {
        let result = resolve_task_with_editor(
            None,
            "test-agent",
            "",
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
    /// at all via the injected `tmp_dir_fn` - pointed here at a directory
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
            None,
            "test-agent",
            "",
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

    // ─── resolve_task_with: editor path (stdin is a TTY) - Windows twins ──

    /// Write a `.bat` stand-in editor. CRLF and `@echo off` because `cmd`
    /// wants both.
    #[cfg(windows)]
    fn write_bat(path: &std::path::Path, body: &str) {
        std::fs::write(path, format!("@echo off\r\n{}\r\n", body)).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn resolve_task_with_editor_path_happy_case() {
        // A tiny batch "editor" that appends a non-comment line to whatever
        // file it's invoked on (%~1) - standing in for a real interactive
        // editor session. `%~1` strips any surrounding quotes Windows adds
        // around a path containing spaces.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let script = dir.join("fake-editor.bat");
        write_bat(&script, "echo task body from editor>>\"%~1\"");

        temp_env::with_vars([("VISUAL", Some(&script)), ("EDITOR", None)], || {
            let result = resolve_task_with(None, "test-agent", "a non-empty description", &|| true);
            assert_eq!(result.unwrap(), "task body from editor");
        });
    }

    #[cfg(windows)]
    #[test]
    fn resolve_task_with_editor_path_empty_after_stripping_comments_errors() {
        // A no-op batch file "opens" the file and does nothing to it, so
        // only the commented-out template remains - stripped down to an
        // empty task.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let ok_bat = dir.join("ok.bat");
        write_bat(&ok_bat, "exit /b 0");

        temp_env::with_vars([("VISUAL", Some(&ok_bat)), ("EDITOR", None)], || {
            let result = resolve_task_with(None, "test-agent", "", &|| true);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Aborting run"));
        });
    }

    // ─── build_task_template: description branch ──────────────────────────

    /// A blueprint with no `description` gets `""` from the manifest parser,
    /// which must not become a bare `# ` line in the template.
    #[test]
    fn build_task_template_with_empty_description_skips_desc_line() {
        let t = build_task_template("agent", "");
        assert!(!t.contains("# \n"));
        assert!(t.contains("# Task for agent: agent\n"));
        assert!(t.contains("Describe your task below"));
    }

    #[test]
    fn build_task_template_with_non_empty_description_adds_desc_line() {
        let t = build_task_template("my-agent", "Build a web server");
        assert!(t.contains("# Task for agent: my-agent\n"));
        assert!(t.contains("# Build a web server\n"));
    }

    // ─── write_task_template: error path ─────────────────────────────────

    /// A temp file that cannot be created is reported rather than swallowed.
    #[test]
    fn write_task_template_error_on_bad_path() {
        // A directory that is not one: creation fails, which is the single
        // error this reports.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let result = write_task_template(&blocker, "content");
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
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("secret.txt");
        std::fs::write(&file, "secret content").unwrap();
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&file, perms).unwrap();

        let result = resolve_task_with(Some(file.to_str().unwrap()), "test-agent", "", &|| false);
        // Restore perms before asserting (so cleanup works)
        let mut perms2 = std::fs::metadata(&file).unwrap().permissions();
        perms2.set_mode(0o644);
        std::fs::set_permissions(&file, perms2).ok();

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
    // the handle stays open - a deterministic Windows-native way to force
    // the same "file exists but can't be read" outcome the Unix test above
    // produces via `chmod 000`.
    #[cfg(windows)]
    #[test]
    fn resolve_task_unreadable_file_returns_error() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("secret.txt");
        std::fs::write(&file, "secret content").unwrap();

        // Hold an exclusive (no-share) handle open for the duration of the
        // read attempt below.
        let _locked = OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&file)
            .unwrap();

        let result = resolve_task_with(Some(file.to_str().unwrap()), "test-agent", "", &|| false);

        drop(_locked);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read task file")
        );
    }
}
