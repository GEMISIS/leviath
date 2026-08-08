//! Opening the user's text editor on a file and waiting for it to close.
//!
//! The fallback editor list differs per OS (`vim`/`nano`/`vi` against
//! `edit`/`notepad`). As in [`crate::browser`], the platform selection is a
//! pure function taking the OS string, the candidate list and argv split are
//! pure, and the actual process run is injected - so nothing here needs a
//! `#[cfg]` and every branch is reachable under test on a single platform.
//!
//! What is *not* here is anything about what the file contains. Building the
//! task template, stripping its comment lines and deciding whether an empty
//! result cancels the run are Leviath policy, not OS behavior, and live in
//! `leviath-cli`.

use std::path::Path;
use std::process::Command;

/// Outcome of running one editor candidate, abstracting over the raw
/// `ExitStatus`. This exists so the "ran but ended with no exit code" case (a
/// signal kill on Unix) is injectable in tests on *every* platform: on Windows
/// an `ExitStatus` always carries a code (even via `ExitStatusExt::from_raw`),
/// so that case cannot be fabricated from a status directly. The injected `run`
/// seam of [`launch_via`] therefore yields this enum rather than an
/// `ExitStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorRunOutcome {
    /// Process finished (success, or any explicit exit code) - treat as the
    /// user having closed the editor.
    Completed,
    /// Process ended with no exit code (e.g. killed by a signal) - try the next
    /// candidate.
    Aborted,
}

/// The fallback editor candidates for `os`, tried in order after any
/// `$VISUAL`/`$EDITOR` value.
///
/// `os` is the value of `std::env::consts::OS`. An unrecognized OS gets the
/// Unix list, which covers the BSDs and other Unixes - the same fallback shape
/// as [`crate::browser::open_command_for`].
///
/// On Windows `edit` (Microsoft Edit, shipped with Windows 11 since 2025) comes
/// first because it is a *console* editor: it stays in the terminal the user
/// typed the command into, and it works over SSH and in containers where a
/// notepad window does not. Listing it first costs nothing where it is absent,
/// since an unresolvable program is an `ErrorKind::NotFound` that [`launch_via`]
/// skips. `notepad` is the guaranteed fallback (Windows resolves it through the
/// System32 search path whatever `$PATH` says, and unlike `start notepad` it
/// blocks until the window closes). `vim` last picks up Git-for-Windows and
/// scoop installs for users who never set `$EDITOR`.
pub fn default_editors_for(os: &str) -> Vec<&'static str> {
    match os {
        "windows" => vec!["edit", "notepad", "vim"],
        _ => vec!["vim", "nano", "vi"],
    }
}

/// The full candidate list in priority order: `$VISUAL`, then `$EDITOR`, then
/// the platform fallbacks for `os`.
///
/// The two environment values are parameters rather than reads, so this stays
/// pure and every combination is testable without touching the process
/// environment. An unset *or* empty value contributes nothing: an exported but
/// empty `EDITOR=` is a common shell-profile accident, and treating it as a
/// program name would spawn nothing and mask the real fallbacks.
pub fn editor_candidates(visual: Option<&str>, editor: Option<&str>, os: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for preferred in [visual, editor] {
        if let Some(value) = preferred
            && !value.is_empty()
        {
            candidates.push(value.to_string());
        }
    }
    candidates.extend(default_editors_for(os).into_iter().map(str::to_string));
    candidates
}

/// Split one candidate into a program and its arguments, with `path` appended.
///
/// Candidates are split on whitespace so an editor string carrying flags
/// (`code --wait`) works. The consequence, which callers should document: a
/// program *path* containing spaces is split in the wrong place and needs a
/// wrapper script on `PATH` instead.
///
/// `None` when the candidate has no program token at all, which is what a
/// whitespace-only value amounts to.
pub fn editor_argv(candidate: &str, path: &str) -> Option<(String, Vec<String>)> {
    let mut parts = candidate.split_whitespace();
    let program = parts.next()?;
    let mut args: Vec<String> = parts.map(str::to_string).collect();
    args.push(path.to_string());
    Some((program.to_string(), args))
}

/// Classify an editor subprocess's exit. `code == None` means it ended without
/// an exit code (a signal kill). A pure function so both arms are unit-testable
/// on every platform, independent of whether a real process can produce a
/// code-less status there.
pub fn classify_exit(success: bool, code: Option<i32>) -> EditorRunOutcome {
    if success || code.is_some() {
        EditorRunOutcome::Completed
    } else {
        EditorRunOutcome::Aborted
    }
}

/// Try each candidate in order and return once one runs to completion.
///
/// `run` is injected so every arm - including "no editor found" - is reachable
/// under test on every platform without spawning a real, blocking, interactive
/// editor. That matters most on Windows: `Command::new("notepad")` resolves
/// through the System32 search path that `CreateProcess` consults *before*
/// `$PATH`, so it cannot be made to fail short of tampering with a real system
/// directory, and letting it actually open would hang CI with no timeout.
///
/// `run` is `&mut dyn FnMut` rather than `impl FnMut` because several test call
/// sites pass distinct closure literals, and a generic parameter would give
/// each one its own coverage-mapping instantiation. `cargo llvm-cov` sometimes
/// reports a region as uncovered for one instantiation even when the union of
/// all of them covers every source position.
pub fn launch_via(
    path: &Path,
    candidates: &[String],
    run: &mut dyn FnMut(&mut Command) -> std::io::Result<EditorRunOutcome>,
) -> std::io::Result<()> {
    let path_str = path.to_string_lossy();

    for candidate in candidates {
        let Some((program, args)) = editor_argv(candidate, path_str.as_ref()) else {
            continue;
        };
        // The one child meant to be seen: it inherits this process's stdio and
        // draws in the user's terminal, so starting `vim` or `edit` without a
        // console would leave it nowhere to draw and nothing to read from.
        // `terminal_command` says that, where a bare `Command::new` would only
        // have looked like a forgotten `child_command`.
        let mut cmd = crate::process::terminal_command(program);
        cmd.args(args);

        match run(&mut cmd) {
            // Exited, even non-zero: the user closed the editor.
            Ok(EditorRunOutcome::Completed) => return Ok(()),
            // Ended with no exit code (a signal kill) - try the next candidate.
            Ok(EditorRunOutcome::Aborted) => {}
            // Not installed - try the next candidate.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "Failed to launch editor '{candidate}': {e}"
                )));
            }
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "No editor found. Set $VISUAL or $EDITOR, or install vim, nano, or edit.",
    ))
}

/// Launch the user's editor on `path` and wait for it to close.
///
/// The only impure function here: it reads `$VISUAL`/`$EDITOR` and runs a real
/// subprocess. Everything it decides is delegated to the pure functions above.
pub fn launch(path: &Path) -> std::io::Result<()> {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    let candidates = editor_candidates(visual.as_deref(), editor.as_deref(), std::env::consts::OS);
    launch_via(path, &candidates, &mut |cmd| {
        cmd.status().map(|s| classify_exit(s.success(), s.code()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// A path that never has to exist: nothing here opens the file, and the
    /// injected `run` never spawns.
    fn some_path() -> std::path::PathBuf {
        std::path::PathBuf::from("/lev/task.txt")
    }

    #[test]
    fn windows_prefers_the_console_editor_then_notepad() {
        assert_eq!(
            default_editors_for("windows"),
            vec!["edit", "notepad", "vim"]
        );
    }

    #[test]
    fn unix_and_unknown_oses_get_the_same_list() {
        assert_eq!(default_editors_for("linux"), vec!["vim", "nano", "vi"]);
        assert_eq!(default_editors_for("macos"), vec!["vim", "nano", "vi"]);
        // An OS string nobody special-cased still gets a usable list.
        assert_eq!(default_editors_for("dragonfly"), vec!["vim", "nano", "vi"]);
    }

    #[test]
    fn visual_comes_before_editor_and_both_before_the_defaults() {
        assert_eq!(
            editor_candidates(Some("code --wait"), Some("nvim"), "linux"),
            owned(&["code --wait", "nvim", "vim", "nano", "vi"])
        );
    }

    #[test]
    fn an_unset_visual_or_editor_contributes_nothing() {
        assert_eq!(
            editor_candidates(None, Some("nvim"), "linux"),
            owned(&["nvim", "vim", "nano", "vi"])
        );
        assert_eq!(
            editor_candidates(Some("nvim"), None, "linux"),
            owned(&["nvim", "vim", "nano", "vi"])
        );
        assert_eq!(
            editor_candidates(None, None, "windows"),
            owned(&["edit", "notepad", "vim"])
        );
    }

    /// An exported but empty `EDITOR=` is a common shell-profile accident. It
    /// must not shadow the real fallbacks with a program name of "".
    #[test]
    fn an_empty_visual_or_editor_is_skipped() {
        assert_eq!(
            editor_candidates(Some(""), Some(""), "linux"),
            owned(&["vim", "nano", "vi"])
        );
    }

    #[test]
    fn editor_argv_splits_flags_and_appends_the_path() {
        let (program, args) = editor_argv("code --wait --new-window", "/tmp/t.txt").unwrap();
        assert_eq!(program, "code");
        assert_eq!(args, owned(&["--wait", "--new-window", "/tmp/t.txt"]));
    }

    #[test]
    fn editor_argv_appends_the_path_to_a_bare_program() {
        let (program, args) = editor_argv("vim", "/tmp/t.txt").unwrap();
        assert_eq!(program, "vim");
        assert_eq!(args, owned(&["/tmp/t.txt"]));
    }

    #[test]
    fn editor_argv_rejects_a_candidate_with_no_program_token() {
        assert!(editor_argv("   ", "/tmp/t.txt").is_none());
        assert!(editor_argv("", "/tmp/t.txt").is_none());
    }

    #[test]
    fn classify_exit_treats_success_as_completed() {
        assert_eq!(classify_exit(true, Some(0)), EditorRunOutcome::Completed);
    }

    #[test]
    fn classify_exit_treats_a_nonzero_code_as_completed() {
        // A non-zero but present exit code means the user closed the editor.
        assert_eq!(classify_exit(false, Some(1)), EditorRunOutcome::Completed);
    }

    #[test]
    fn classify_exit_treats_a_missing_code_as_aborted() {
        // No exit code (killed by a Unix signal) means try the next candidate.
        assert_eq!(classify_exit(false, None), EditorRunOutcome::Aborted);
    }

    /// `Debug` is derived and used by the `assert_eq!`s above only when they
    /// fail, so exercise it directly rather than leaving it to a failing run.
    #[test]
    fn the_outcome_enum_formats_both_variants() {
        assert_eq!(format!("{:?}", EditorRunOutcome::Completed), "Completed");
        assert_eq!(format!("{:?}", EditorRunOutcome::Aborted), "Aborted");
        // `Clone` is derived alongside `Copy`; call it so it is not an
        // uncovered function.
        assert_eq!(EditorRunOutcome::Aborted.clone(), EditorRunOutcome::Aborted);
    }

    #[test]
    fn launch_via_returns_on_the_first_candidate_that_completes() {
        let mut seen: Vec<String> = Vec::new();
        let result = launch_via(&some_path(), &owned(&["code --wait", "vim"]), &mut |cmd| {
            seen.push(cmd.get_program().to_string_lossy().to_string());
            Ok(EditorRunOutcome::Completed)
        });
        assert!(result.is_ok());
        // Only the first candidate ran, and it ran with its flag plus the path.
        assert_eq!(seen, owned(&["code"]));
    }

    #[test]
    fn launch_via_passes_the_flags_and_the_path_through_to_the_command() {
        let mut args: Vec<String> = Vec::new();
        let result = launch_via(&some_path(), &owned(&["code --wait"]), &mut |cmd| {
            args = cmd
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            Ok(EditorRunOutcome::Completed)
        });
        assert!(result.is_ok());
        assert_eq!(args, owned(&["--wait", "/lev/task.txt"]));
    }

    #[test]
    fn launch_via_skips_a_candidate_with_no_program_token() {
        let mut seen: Vec<String> = Vec::new();
        let result = launch_via(&some_path(), &owned(&["   ", "vim"]), &mut |cmd| {
            seen.push(cmd.get_program().to_string_lossy().to_string());
            Ok(EditorRunOutcome::Completed)
        });
        assert!(result.is_ok());
        // The whitespace-only candidate never reached the runner.
        assert_eq!(seen, owned(&["vim"]));
    }

    #[test]
    fn launch_via_tries_the_next_candidate_after_an_abort() {
        let mut seen: Vec<String> = Vec::new();
        let result = launch_via(&some_path(), &owned(&["a", "b"]), &mut |cmd| {
            let program = cmd.get_program().to_string_lossy().to_string();
            seen.push(program.clone());
            if program == "a" {
                Ok(EditorRunOutcome::Aborted)
            } else {
                Ok(EditorRunOutcome::Completed)
            }
        });
        assert!(result.is_ok());
        assert_eq!(seen, owned(&["a", "b"]));
    }

    #[test]
    fn launch_via_tries_the_next_candidate_when_one_is_not_installed() {
        let mut seen: Vec<String> = Vec::new();
        let result = launch_via(&some_path(), &owned(&["a", "b"]), &mut |cmd| {
            let program = cmd.get_program().to_string_lossy().to_string();
            seen.push(program.clone());
            if program == "a" {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such file",
                ))
            } else {
                Ok(EditorRunOutcome::Completed)
            }
        });
        assert!(result.is_ok());
        assert_eq!(seen, owned(&["a", "b"]));
    }

    /// A spawn failure that is *not* "the program is missing" - a permission
    /// denial, say - is the user's actual problem and must be reported rather
    /// than silently skipped in favour of some other editor.
    #[test]
    fn launch_via_reports_a_spawn_failure_that_is_not_a_missing_program() {
        let result = launch_via(&some_path(), &owned(&["locked-editor"]), &mut |_cmd| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Failed to launch editor 'locked-editor'"),
            "{err}"
        );
    }

    /// Both routes to the terminal error: running out of candidates, and never
    /// having any. One shared runner rather than two closures, because a
    /// closure written only for the empty-list call would never be invoked and
    /// so would itself be uncovered.
    #[test]
    fn launch_via_reports_no_editor_when_the_candidates_run_out() {
        let mut runner = |_cmd: &mut Command| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ))
        };

        let err = launch_via(&some_path(), &owned(&["a", "b"]), &mut runner).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().starts_with("No editor found."), "{err}");

        let err = launch_via(&some_path(), &[], &mut runner).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().starts_with("No editor found."), "{err}");
    }

    /// Drives the real [`launch`]: real environment reads, a real
    /// `Command::status()`, and a real [`classify_exit`] on the result.
    ///
    /// `$VISUAL` points at this very test binary with `--list`, so the
    /// "editor" is a process that is guaranteed to exist on every platform,
    /// exits immediately, and runs no tests (`--list` only prints names, and
    /// the appended file path acts as a filter that matches none of them). It
    /// is the *first* candidate, so no real editor is ever reached. `temp_env`
    /// serializes environment mutation process-wide, which is required because
    /// `std::env::set_var` is unsafe and this crate forbids unsafe.
    #[test]
    fn launch_runs_the_first_candidate_and_reports_it_completed() {
        let exe = std::env::current_exe().expect("test binary path");
        let visual = format!("{} --list", exe.display());
        temp_env::with_vars(
            [("VISUAL", Some(visual.as_str())), ("EDITOR", None)],
            || {
                let dir = tempfile::tempdir().unwrap();
                let file = dir.path().join("task.txt");
                std::fs::write(&file, "content").unwrap();
                launch(&file).expect("the stand-in editor should run to completion");
            },
        );
    }
}
