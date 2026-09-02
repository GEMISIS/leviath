//! Confirming a workdir that is somewhere an agent probably should not write.
//!
//! `lev run`'s workdir defaults to wherever it was invoked, and running from a
//! home directory is an easy accident. An agent turned loose under a profile
//! root can eat tens of gigabytes while doing exactly what it was told, in the
//! directory it was given.
//!
//! So this asks - once, and only about the two shapes that are alarming:
//!
//! - a **home directory** (`~`, `/home/x`, `/Users/x`, `C:\Users\x`), where an
//!   agent's writes land among everything the user owns, and
//! - a **filesystem root** (`/`, `C:\`), where they land among everything.
//!
//! Anything else - a project directory, a scratch dir, a repo checkout - passes
//! without a word. This is deliberately not an allowlist that must be populated
//! before leviath is usable: a tool that asks about everything trains people to
//! say yes to everything, which is the failure mode it would be trying to stop.
//!
//! With no terminal to ask on - CI, a pipe, `--yolo` - the run **proceeds**
//! with a warning rather than being refused; breaking every unattended
//! caller to enforce a prompt would trade one failure mode for a worse one.
//!
//! The decision is a pure function over paths (`assess`) so it can be tested
//! without a filesystem or a terminal; asking the question is the caller's.

/// What to do about a run's workdir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkdirVerdict {
    /// Nothing alarming, or the user has already said they work here.
    Proceed,
    /// Worth confirming. Carries what to tell the user.
    Confirm(WorkdirConcern),
}

/// Why a workdir was questioned. Separate from the message so the caller can
/// render it as a prompt, a refusal, or a log line without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkdirConcern {
    /// The workdir is the user's home directory itself.
    HomeDirectory,
    /// The workdir is a filesystem root.
    FilesystemRoot,
}

impl WorkdirConcern {
    /// One line saying what is alarming about it.
    pub(crate) fn headline(&self) -> &'static str {
        match self {
            Self::HomeDirectory => "That is your home directory.",
            Self::FilesystemRoot => "That is a filesystem root.",
        }
    }

    /// What an agent could do there, concretely rather than in the abstract.
    pub(crate) fn detail(&self) -> &'static str {
        match self {
            Self::HomeDirectory => {
                "An agent's file tools are confined to its workdir, so this run could read \
                 and write anything in your home - including SSH keys, browser data, and \
                 every other project you have."
            }
            Self::FilesystemRoot => {
                "An agent's file tools are confined to its workdir, so this run would be \
                 confined to the whole machine."
            }
        }
    }
}

/// Decide whether `workdir` needs confirming.
///
/// `home` is the user's home directory (`None` when it cannot be resolved, in
/// which case the home check simply cannot fire). `allowed` is
/// `[security] allowed_workdirs`; a workdir at or under any entry proceeds.
///
/// Comparison is textual on already-canonicalised paths - `effective_workdir`
/// canonicalises the `--workdir` flag, and the invocation directory is
/// canonical by construction. This deliberately does not touch the filesystem:
/// the check runs on every `lev run`, and a stat storm on the startup path
/// would be a poor trade for catching a symlinked home.
pub(crate) fn assess(
    workdir: &std::path::Path,
    home: Option<&std::path::Path>,
    allowed: &[String],
) -> WorkdirVerdict {
    if allowed.iter().any(|a| is_within(workdir, a.as_ref())) {
        return WorkdirVerdict::Proceed;
    }
    if workdir.parent().is_none() {
        return WorkdirVerdict::Confirm(WorkdirConcern::FilesystemRoot);
    }
    if home.is_some_and(|h| h == workdir) {
        return WorkdirVerdict::Confirm(WorkdirConcern::HomeDirectory);
    }
    WorkdirVerdict::Proceed
}

/// The home directory as the filesystem knows it, for a caller that compares
/// it against a canonicalised workdir.
///
/// `assess` is textual, so a home that is itself a symlink (Fedora
/// Silverblue's `/home` is a link to `var/home`; a `dirs::home_dir()` there
/// answers `/home/alice` while the canonical workdir is `/var/home/alice`)
/// would never match the workdir and the guard would never fire. A home that
/// cannot be canonicalised (it does not exist, or is not readable) is kept
/// as given, so the check still fires on the spelling it has.
pub fn canonical_home(home: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf())
}

/// Whether `path` is `base` or sits under it.
///
/// Component-wise rather than a string prefix: `/home/alice-old` starts with
/// `/home/alice` as text and is a different directory.
fn is_within(path: &std::path::Path, base: &std::path::Path) -> bool {
    // An empty entry would otherwise match everything, silencing the guard for
    // every workdir - a typo in the config should not disable it.
    if base.as_os_str().is_empty() {
        return false;
    }
    path.starts_with(base)
}

/// What to warn when there is no terminal to ask on.
///
/// The run **proceeds**. Refusing would break every unattended caller - CI, a
/// pipe, `--yolo` - and a prompt nobody can answer is worse still, because it
/// parks the run until something times it out and reads as a hang.
///
/// The cost of that choice is that the guard is advisory in exactly the
/// unattended case that has the most to lose, so this line is the whole
/// mitigation: it goes to stderr on every such run, names the directory, and
/// says how to silence it. Someone reading the log afterwards should be able
/// to find the moment an agent was pointed at a home directory.
pub(crate) fn non_interactive_warning(
    workdir: &std::path::Path,
    concern: &WorkdirConcern,
) -> String {
    format!(
        "warning: running in '{}'. {} {}\n\
         Proceeding without confirmation - there is no terminal to ask on. Silence this by \
         adding it to your config:\n\n\
         [security]\nallowed_workdirs = [\"{}\"]\n\n\
         Or pass --workdir to run somewhere else.",
        workdir.display(),
        concern.headline(),
        concern.detail(),
        workdir.display(),
    )
}

// ─── Asking ──────────────────────────────────────────────────────────────────

/// Put the question on screen and wait for an answer.
///
/// Generic over the same [`crate::tui::TerminalSetup`]/[`crate::tui::EventSource`] seams `lev setup`
/// uses, so the whole flow runs against a `TestBackend` with canned keys - the
/// real crossterm pair lives in the binary, where the terminal I/O belongs.
///
/// Returns whether to proceed. Anything that is not an explicit yes is a no:
/// Esc, `n`, a closed event source, or a draw that fails. A confirmation that
/// defaults to yes on an error is not a confirmation.
pub(crate) async fn confirm_core<S: crate::tui::TerminalSetup, E: crate::tui::EventSource>(
    workdir: &std::path::Path,
    concern: &WorkdirConcern,
    setup: &mut S,
    events: &mut E,
) -> bool {
    use crate::tui::widgets::confirm::{Confirm, ConfirmOutcome};
    use ratatui::text::Line;

    let mut dialog = Confirm::new(
        "Confirm working directory",
        vec![
            Line::from(format!("{}", workdir.display())),
            Line::from(""),
            Line::from(concern.headline()),
            Line::from(""),
            Line::from(concern.detail()),
            Line::from(""),
            Line::from("Add it to [security] allowed_workdirs to stop being asked."),
        ],
        "Run here",
        "Cancel",
    )
    .danger();

    if setup.enable().is_err() {
        return false;
    }
    let Ok(mut terminal) = setup.create_terminal() else {
        setup.disable();
        return false;
    };

    let answer = loop {
        if terminal.draw(|f| dialog.draw(f, f.area())).is_err() {
            break false;
        }
        match events.poll_event(std::time::Duration::from_millis(120)) {
            Ok(Some(crossterm::event::Event::Key(key)))
                if key.kind == crossterm::event::KeyEventKind::Press =>
            {
                match dialog.handle(&key) {
                    ConfirmOutcome::Yes => break true,
                    ConfirmOutcome::No => break false,
                    ConfirmOutcome::Pending => {}
                }
            }
            // A tick, a resize, a key release: keep drawing and asking.
            Ok(_) => {}
            // The event source is gone. Nobody is going to answer, and the safe
            // answer is the one that does not run.
            Err(_) => break false,
        }
    };

    setup.disable();
    answer
}

/// The whole check, for `lev run`: assess, then ask or warn.
///
/// Returns whether the run may proceed. `interactive` is whether there is a
/// terminal to ask on - when there is not (CI, a pipe, `--yolo`), the run
/// proceeds with `non_interactive_warning` on stderr rather than being
/// refused, because refusing would break every unattended caller.
pub async fn check<S: crate::tui::TerminalSetup, E: crate::tui::EventSource>(
    workdir: &std::path::Path,
    home: Option<&std::path::Path>,
    allowed: &[String],
    interactive: bool,
    setup: &mut S,
    events: &mut E,
) -> bool {
    let WorkdirVerdict::Confirm(concern) = assess(workdir, home, allowed) else {
        return true;
    };
    if !interactive {
        eprintln!("{}", non_interactive_warning(workdir, &concern));
        return true;
    }
    confirm_core(workdir, &concern, setup, events).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn home() -> Option<&'static Path> {
        Some(Path::new("/Users/alice"))
    }

    #[test]
    fn an_ordinary_project_directory_passes() {
        assert_eq!(
            assess(Path::new("/Users/alice/code/leviath"), home(), &[]),
            WorkdirVerdict::Proceed
        );
    }

    #[test]
    fn the_home_directory_itself_is_questioned() {
        assert_eq!(
            assess(Path::new("/Users/alice"), home(), &[]),
            WorkdirVerdict::Confirm(WorkdirConcern::HomeDirectory)
        );
    }

    /// A directory *inside* home is the normal case and must not prompt -
    /// otherwise the guard fires on nearly every run and stops being read.
    #[test]
    fn a_directory_under_home_is_not_questioned() {
        assert_eq!(
            assess(Path::new("/Users/alice/projects"), home(), &[]),
            WorkdirVerdict::Proceed
        );
    }

    #[test]
    fn a_filesystem_root_is_questioned() {
        assert_eq!(
            assess(Path::new("/"), home(), &[]),
            WorkdirVerdict::Confirm(WorkdirConcern::FilesystemRoot)
        );
    }

    #[test]
    fn an_allowed_directory_proceeds_even_when_it_is_home() {
        assert_eq!(
            assess(
                Path::new("/Users/alice"),
                home(),
                &["/Users/alice".to_string()]
            ),
            WorkdirVerdict::Proceed
        );
    }

    #[test]
    fn an_allowed_directory_covers_what_is_under_it() {
        assert_eq!(
            assess(Path::new("/"), home(), &["/".to_string()]),
            WorkdirVerdict::Proceed
        );
    }

    /// Textual prefixes are not enough: these are different directories.
    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_allowed_by_it() {
        assert_eq!(
            assess(
                Path::new("/Users/alice-old"),
                Some(Path::new("/Users/alice-old")),
                &["/Users/alice".to_string()]
            ),
            WorkdirVerdict::Confirm(WorkdirConcern::HomeDirectory)
        );
    }

    /// A typo that produced an empty entry must not silence the guard for
    /// every workdir.
    #[test]
    fn an_empty_allowed_entry_matches_nothing() {
        assert_eq!(
            assess(Path::new("/Users/alice"), home(), &[String::new()]),
            WorkdirVerdict::Confirm(WorkdirConcern::HomeDirectory)
        );
    }

    #[test]
    fn without_a_resolvable_home_the_home_check_cannot_fire() {
        assert_eq!(
            assess(Path::new("/Users/alice"), None, &[]),
            WorkdirVerdict::Proceed
        );
    }

    /// A home that exists comes back as the filesystem spells it (on macOS the
    /// tempdir under `/var` canonicalises to `/private/var`), so a symlinked
    /// home matches a canonicalised workdir.
    #[test]
    fn canonical_home_resolves_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            canonical_home(dir.path()),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    /// A home that does not exist is kept as given rather than dropped: the
    /// textual check still fires on the spelling it has.
    #[test]
    fn canonical_home_keeps_a_path_it_cannot_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nowhere");
        assert_eq!(canonical_home(&missing), missing);
    }

    /// The case the helper exists for: home reached through a symlink, the
    /// workdir given as the real directory. Textually different, the same
    /// place, and the guard has to say so.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_home_is_still_questioned_once_canonicalised() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real-home");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.path().join("home-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let workdir = std::fs::canonicalize(&real).unwrap();
        // Raw, the symlink never matches the canonical workdir.
        assert_eq!(assess(&workdir, Some(&link), &[]), WorkdirVerdict::Proceed);
        assert_eq!(
            assess(&workdir, Some(&canonical_home(&link)), &[]),
            WorkdirVerdict::Confirm(WorkdirConcern::HomeDirectory)
        );
    }

    #[test]
    fn both_concerns_explain_themselves() {
        for c in [
            WorkdirConcern::HomeDirectory,
            WorkdirConcern::FilesystemRoot,
        ] {
            assert!(c.headline().ends_with('.'), "{c:?}");
            assert!(c.detail().contains("confined"), "{c:?}");
        }
    }

    /// The warning is the whole mitigation on the unattended path, so it has to
    /// carry all three things someone needs: where it ran, that it was not
    /// confirmed, and how to stop being asked.
    #[test]
    fn the_warning_names_the_directory_the_choice_and_the_fix() {
        let msg =
            non_interactive_warning(Path::new("/Users/alice"), &WorkdirConcern::HomeDirectory);
        assert!(msg.contains("/Users/alice"), "{msg}");
        assert!(msg.contains("Proceeding without confirmation"), "{msg}");
        assert!(
            msg.contains("allowed_workdirs = [\"/Users/alice\"]"),
            "{msg}"
        );
        assert!(msg.contains("--workdir"), "{msg}");
    }

    // ─── the dialog ───────────────────────────────────────────────────────

    use crate::tui::{TestEventSource, TestSetup};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    async fn ask(events: Vec<Option<Event>>) -> bool {
        confirm_core(
            Path::new("/Users/alice"),
            &WorkdirConcern::HomeDirectory,
            &mut TestSetup::new(),
            &mut TestEventSource::new_with_nones(events),
        )
        .await
    }

    #[tokio::test]
    async fn y_runs_here() {
        assert!(ask(vec![Some(key(KeyCode::Char('y')))]).await);
    }

    #[tokio::test]
    async fn n_cancels() {
        assert!(!ask(vec![Some(key(KeyCode::Char('n')))]).await);
    }

    #[tokio::test]
    async fn esc_cancels() {
        assert!(!ask(vec![Some(key(KeyCode::Esc))]).await);
    }

    /// Focus starts on Cancel, so a bare Enter must not run. This is the whole
    /// point of using the two-button dialog rather than "y accepts, anything
    /// else dismisses".
    #[tokio::test]
    async fn enter_alone_takes_the_safe_answer() {
        assert!(!ask(vec![Some(key(KeyCode::Enter))]).await);
    }

    #[tokio::test]
    async fn moving_focus_then_entering_runs_here() {
        assert!(ask(vec![Some(key(KeyCode::Right)), Some(key(KeyCode::Enter)),]).await);
    }

    /// Ticks with no input keep the dialog up rather than answering it.
    #[tokio::test]
    async fn a_quiet_poll_does_not_answer() {
        assert!(ask(vec![None, None, Some(key(KeyCode::Char('y')))]).await);
    }

    /// Every way of failing to ask resolves to "do not run". A confirmation
    /// that defaults to yes when it cannot be shown is not a confirmation.
    #[tokio::test]
    async fn a_terminal_that_will_not_enable_cancels() {
        let mut setup = TestSetup::new();
        setup.enable_should_fail = true;
        assert!(
            !confirm_core(
                Path::new("/Users/alice"),
                &WorkdirConcern::HomeDirectory,
                &mut setup,
                &mut TestEventSource::new(vec![key(KeyCode::Char('y'))]),
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_terminal_that_will_not_open_cancels() {
        let mut setup = TestSetup::new();
        setup.create_should_fail = true;
        assert!(
            !confirm_core(
                Path::new("/Users/alice"),
                &WorkdirConcern::HomeDirectory,
                &mut setup,
                &mut TestEventSource::new(vec![key(KeyCode::Char('y'))]),
            )
            .await
        );
    }

    /// A terminal that cannot be drawn to cannot have shown the question, so
    /// the answer is no. Same stance as the two failures above.
    #[tokio::test]
    async fn a_terminal_that_cannot_be_drawn_to_cancels() {
        let mut setup = TestSetup::new();
        setup.draw_should_fail = true;
        assert!(
            !confirm_core(
                Path::new("/Users/alice"),
                &WorkdirConcern::HomeDirectory,
                &mut setup,
                &mut TestEventSource::new(vec![key(KeyCode::Char('y'))]),
            )
            .await
        );
    }

    #[tokio::test]
    async fn an_event_source_that_dies_cancels() {
        assert!(
            !confirm_core(
                Path::new("/Users/alice"),
                &WorkdirConcern::HomeDirectory,
                &mut TestSetup::new(),
                &mut TestEventSource::failing(),
            )
            .await
        );
    }

    /// A key *release* is not an answer - on Windows crossterm reports both
    /// press and release, and answering on either would take the first of a
    /// pair as two answers.
    #[tokio::test]
    async fn a_key_release_is_not_an_answer() {
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert!(!ask(vec![Some(release), Some(key(KeyCode::Esc))]).await);
    }

    // ─── the entry point ──────────────────────────────────────────────────

    async fn check_in(dir: &str, interactive: bool, events: Vec<Event>) -> bool {
        check(
            Path::new(dir),
            home(),
            &[],
            interactive,
            &mut TestSetup::new(),
            &mut TestEventSource::new(events),
        )
        .await
    }

    #[tokio::test]
    async fn an_unremarkable_workdir_never_asks() {
        // No events at all: if it tried to ask, the source would run dry and
        // the answer would be "no", so `true` here proves it did not ask.
        assert!(check_in("/Users/alice/code", true, vec![]).await);
    }

    #[tokio::test]
    async fn an_alarming_workdir_asks_when_there_is_a_terminal() {
        assert!(check_in("/Users/alice", true, vec![key(KeyCode::Char('y'))]).await);
        assert!(!check_in("/Users/alice", true, vec![key(KeyCode::Char('n'))]).await);
    }

    /// The unattended path proceeds rather than refusing - breaking CI to
    /// enforce a prompt trades one failure mode for a worse one. Again the
    /// empty event list is the evidence that nothing was asked.
    #[tokio::test]
    async fn without_a_terminal_it_proceeds_rather_than_refusing() {
        assert!(check_in("/Users/alice", false, vec![]).await);
    }

    #[tokio::test]
    async fn an_allowed_workdir_does_not_ask_even_interactively() {
        assert!(
            check(
                Path::new("/Users/alice"),
                home(),
                &["/Users/alice".to_string()],
                true,
                &mut TestSetup::new(),
                &mut TestEventSource::new(vec![]),
            )
            .await
        );
    }
}
