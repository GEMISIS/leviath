//! Where a shell line's path operands point, so a safe-listed read stays a read
//! of the project.
//!
//! The safe list says `cat` and `grep` need no prompt, which is right for
//! `cat src/main.rs` and wrong for `cat ~/.ssh/id_rsa`. `read_file` already
//! refuses the second through `resolve_read`; this holds the shell to the same
//! fence by asking it about every word in the line that could name a file.
//!
//! **It fails closed.** Anything it cannot prove stays inside counts as
//! outside: an expansion, a glob or brace that could reach somewhere else, a
//! `..` the shell might resolve through a symlink, a `cd` it cannot follow, a
//! redirect on a group. A wrong guess costs a prompt, never a read nobody
//! approved. What it cannot do is see inside a program: `python x.py` may read
//! anything, which is why a line that hands control to another interpreter is
//! reported as [`Outside::unplaceable`].

use std::path::{Path, PathBuf};

use super::{BACKSLASH_ESCAPES, Segment, Stripped, Word};

/// What in a line reaches, or might reach, outside the workdir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Outside {
    /// The operands that name somewhere outside, or that this cannot place,
    /// as written. Any of them makes the whole line uncovered: no safe entry
    /// and no grant runs it unprompted.
    pub operands: Vec<String>,
    /// Why what the line does cannot be judged from its words at all, when
    /// that is so: it does not parse, or it hands a command to another
    /// interpreter (`sh -c`, `eval`, `xargs`, `python -c`). Such a line is
    /// refused where `outside_workdir = "deny"`.
    pub unplaceable: Option<String>,
}

impl Outside {
    /// Whether nothing in the line leaves the workdir, as far as its words
    /// show. An unplaceable line still counts as inside here: nothing in its
    /// words left, and whether it runs unprompted is the safe list's and the
    /// grants' question. Only a `deny` refuses it.
    pub(crate) fn is_empty(&self) -> bool {
        self.operands.is_empty()
    }

    /// The reason a `deny` refuses this line, or `None` when it may run.
    pub(crate) fn refusal(&self) -> Option<String> {
        match (&self.unplaceable, self.operands.is_empty()) {
            (Some(why), _) => Some(why.clone()),
            (None, false) => Some(format!("it names {}", self.operands.join(", "))),
            (None, true) => None,
        }
    }
}

/// Programs that open no file named in their arguments, so a literal word is
/// not a path operand. `echo /etc/hosts` prints a string. A word they are
/// handed that expands at run time is still outside: `echo "$(< /etc/hosts)"`
/// reads the file in the substitution.
///
/// Short on purpose, and the same on every platform. `type` is not here: on
/// `cmd.exe` it prints a file.
const NO_FILE_OPERANDS: &[&str] = &[
    "echo", "printf", "seq", "sleep", "true", "false", "pwd", "basename", "dirname", "uname",
    "hostname", "whoami", "id", "ps", "pgrep", "which", "tr",
];

/// Programs that run a command or a script, so what the line reads is decided
/// by something its words do not show. A line running one is unplaceable.
const RUNNERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "csh",
    "tcsh",
    "cmd",
    "powershell",
    "pwsh",
    "xargs",
    "env",
    "exec",
    "command",
    "builtin",
    "nohup",
    "timeout",
    "nice",
    "time",
    "sudo",
    "doas",
    "watch",
    "find",
    "node",
    "deno",
    "bun",
    "perl",
    "ruby",
    "php",
    "lua",
    "osascript",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
];

/// Programs that move the shell, so a later segment's relative paths are
/// relative to somewhere else.
const MOVES_THE_SHELL: &[&str] = &["cd", "pushd"];

/// The most paths a glob may expand to before the word counts as outside
/// unchecked.
const MAX_GLOB_MATCHES: usize = 1000;

/// What in `command` reaches outside `workdir`, as `may_read` judges it.
///
/// `may_read` is the read fence (`BuiltinTools::may_read`: the workdir, then
/// `[read_paths]`), asked about absolute paths. `home` is what `~` expands to.
pub(crate) fn outside_workdir(
    command: &str,
    workdir: &Path,
    home: Option<&Path>,
    may_read: &dyn Fn(&Path) -> bool,
) -> Outside {
    outside_workdir_for(command, BACKSLASH_ESCAPES, workdir, home, may_read)
}

/// [`outside_workdir`] with the shell's escape rule supplied, so both
/// platforms' readings are testable on either. `false` is `cmd.exe`, whose
/// `%VAR%` is an expansion too.
pub(crate) fn outside_workdir_for(
    command: &str,
    backslash_escapes: bool,
    workdir: &Path,
    home: Option<&Path>,
    may_read: &dyn Fn(&Path) -> bool,
) -> Outside {
    let mut walk = Walk {
        bases: vec![workdir.to_path_buf()],
        workdir,
        home,
        may_read,
        cmd: !backslash_escapes,
        left: false,
        out: Outside::default(),
    };
    match super::tokenize_for(command, backslash_escapes) {
        Some(segments) => segments.iter().for_each(|s| walk.segment(s)),
        None => walk.out.unplaceable = Some("it cannot be read as commands".to_string()),
    }
    walk.out
}

/// The state of one line's walk.
struct Walk<'a> {
    /// Every directory a relative path in this segment could be relative to.
    /// The workdir, plus each `cd` target inside it seen so far: which of them
    /// the shell is really in depends on `&&`, `||` and subshells, so a path
    /// has to stay inside from all of them.
    bases: Vec<PathBuf>,
    workdir: &'a Path,
    home: Option<&'a Path>,
    may_read: &'a dyn Fn(&Path) -> bool,
    /// Reading for `cmd.exe`, where `%` opens an expansion.
    cmd: bool,
    /// A `cd` left the workdir, so everything after it runs somewhere this
    /// does not follow.
    left: bool,
    out: Outside,
}

impl Walk<'_> {
    fn segment(&mut self, segment: &Segment) {
        // A redirect's file is read whatever the segment runs, a group or a
        // loop included: `{ cat; } < /etc/hosts`.
        for word in &segment.reads {
            self.check_word(word);
        }
        let words = match super::strip_to_program(&segment.words) {
            Stripped::Program { words, .. } => words,
            Stripped::Unreadable => {
                self.out.unplaceable = Some("it runs a command assembled at run time".to_string());
                return;
            }
            Stripped::NothingRuns(_) => return,
        };
        if self.left {
            self.out
                .operands
                .push("(after a cd out of the working directory)".to_string());
        }
        let program = words[0].text.as_str();
        let name = program_name(program);
        if is_runner(name, &words[1..]) {
            self.out.unplaceable = Some(format!("`{name}` runs a command this cannot read"));
        }
        if MOVES_THE_SHELL.contains(&name) {
            self.move_shell(&words[1..]);
            return;
        }
        // `grep -R` follows every symlink under the directory it searches,
        // and the walk is the program's, out of this check's sight.
        if follows_links(name, &words[1..]) {
            self.out
                .operands
                .push(format!("{name} -R (follows every symlink it meets)"));
        }
        let checks_literals = !NO_FILE_OPERANDS.contains(&name);
        let mut options_ended = false;
        for word in &words[1..] {
            let positional = options_ended;
            options_ended |= word.text == "--";
            if !word.literal || self.expands(word) || checks_literals {
                self.check_arg(word, positional);
            }
        }
    }

    /// A `cd`: its target is a path operand, and where it lands decides what
    /// every later segment's relative paths mean.
    fn move_shell(&mut self, args: &[Word]) {
        // No target is `$HOME`, and `-` is wherever the shell was before;
        // neither is a place this can vouch for.
        let Some(target) = args.iter().find(|w| !w.text.starts_with('-')) else {
            self.left = true;
            self.out.operands.push("cd".to_string());
            return;
        };
        let before = self.out.operands.len();
        self.check_word(target);
        if self.out.operands.len() > before {
            self.left = true;
            return;
        }
        // Checked above, so the target is literal, expands and folds.
        let target = leviath_core::paths::expand_home(&target.text, self.home).unwrap_or_default();
        let moved: Vec<PathBuf> = self
            .bases
            .iter()
            .filter_map(|base| leviath_core::paths::fold_dot_dot(&base.join(&target)))
            .collect();
        self.bases.extend(moved);
    }

    /// Whether a word is expanded by this shell even though the lexer kept it
    /// literal: `%VAR%` on `cmd.exe`.
    fn expands(&self, word: &Word) -> bool {
        self.cmd && word.text.contains('%')
    }

    /// Check one argument, as a flag or a plain word.
    fn check_arg(&mut self, word: &Word, positional: bool) {
        if !word.literal || self.expands(word) || positional {
            return self.check_word(word);
        }
        if candidates(&word.text).any(|text| !self.inside(text)) {
            self.out.operands.push(word.text.clone());
        }
    }

    /// Check one word as a path, adding it to the operands when it can reach
    /// outside.
    fn check_word(&mut self, word: &Word) {
        if !word.literal || self.expands(word) {
            let shown = match word.text.is_empty() {
                true => "a substitution",
                false => word.text.as_str(),
            };
            self.out
                .operands
                .push(format!("{shown} (expanded at run time)"));
            return;
        }
        if !self.inside(&word.text) {
            self.out.operands.push(word.text.clone());
        }
    }

    /// Whether `text`, as a path, stays where `may_read` allows from every
    /// base the shell could be in.
    fn inside(&self, text: &str) -> bool {
        // A brace list is expanded by the shell into words this never sees.
        if text.contains('{') && (text.contains(',') || text.contains("..")) {
            return false;
        }
        // `~name` is somebody else's home, which nothing here looks up.
        if text.starts_with('~') && text != "~" && !text.starts_with("~/") {
            return false;
        }
        let Some(path) = leviath_core::paths::expand_home(text, self.home) else {
            return false;
        };
        self.bases.iter().all(|base| {
            expansions(base, &path).is_some_and(|paths| paths.iter().all(|p| self.reads_inside(p)))
        })
    }

    /// Whether the shell, opening `path`, opens something `may_read` allows.
    ///
    /// Two checks. The fence folds `..` lexically, but the shell resolves it
    /// physically, so `link/../secret` with `link` pointing elsewhere opens a
    /// file beside the link's target. A path with a `..` in it is therefore
    /// also canonicalized as written and held to the workdir.
    fn reads_inside(&self, path: &Path) -> bool {
        let climbs = path
            .components()
            .any(|c| c == std::path::Component::ParentDir);
        (self.may_read)(path) && (!climbs || leviath_core::resolves_within(path, self.workdir))
    }
}

/// The paths a word names once the shell has expanded its glob, or `None`
/// when that cannot be checked: a pattern that could match `..`, which a
/// directory listing never shows but the shell does, or more matches than
/// [`MAX_GLOB_MATCHES`]. A glob that matches nothing is passed on as written,
/// as the shell does.
fn expansions(base: &Path, word: &Path) -> Option<Vec<PathBuf>> {
    let path = base.join(word);
    if !word.to_string_lossy().contains(['*', '?', '[']) {
        return Some(vec![path]);
    }
    let dots = glob::MatchOptions {
        require_literal_leading_dot: true,
        ..Default::default()
    };
    let climbs = word.components().any(|c| {
        glob::Pattern::new(&c.as_os_str().to_string_lossy())
            .is_ok_and(|p| p.matches_with("..", dots))
    });
    // `glob` cannot read a Windows verbatim path, which is what a canonical
    // workdir is there, so the prefix comes off for the match and goes back
    // on each result. A base that is not absolute without it (a verbatim UNC
    // share) cannot be matched, so the word is outside.
    let base_text = base.to_string_lossy();
    let (prefix, rest) = split_verbatim(&base_text);
    if climbs || !Path::new(rest).is_absolute() {
        return None;
    }
    // The base is a real directory whose name may hold a `[`; only the word
    // is a pattern.
    let (prefix, pattern) = match word.is_absolute() {
        true => ("", word.to_string_lossy().to_string()),
        false => (
            prefix,
            format!("{}/{}", glob::Pattern::escape(rest), word.to_string_lossy()),
        ),
    };
    let found: Vec<PathBuf> = glob::glob_with(&pattern, dots)
        .into_iter()
        .flatten()
        .flatten()
        .take(MAX_GLOB_MATCHES + 1)
        .map(|p| PathBuf::from(format!("{prefix}{}", p.display())))
        .collect();
    match found.len() {
        0 => Some(vec![path.to_path_buf()]),
        n if n > MAX_GLOB_MATCHES => None,
        _ => Some(found),
    }
}

/// The prefix a Windows verbatim path carries.
const VERBATIM: &str = r"\\?\";

/// A path's Windows verbatim prefix and the rest of it, or no prefix.
fn split_verbatim(path: &str) -> (&str, &str) {
    path.strip_prefix(VERBATIM)
        .map_or(("", path), |rest| (VERBATIM, rest))
}

/// The program's own name, without the directory it was named from.
fn program_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// Whether `name` with `args` runs a command or a script this cannot read.
fn is_runner(name: &str, args: &[Word]) -> bool {
    RUNNERS.contains(&name)
        || name.starts_with("python")
        // `git -c alias.x='!cmd' x` runs a shell command.
        || (name == "git" && args.iter().any(|w| w.text.starts_with("-c")))
}

/// Whether a `grep` is told to follow symlinks as it recurses.
fn follows_links(name: &str, args: &[Word]) -> bool {
    ["grep", "egrep", "fgrep"].contains(&name)
        && args.iter().any(|w| {
            w.text == "--dereference-recursive"
                || (w.text.starts_with('-') && !w.text.starts_with("--") && w.text.contains('R'))
        })
}

/// The paths one argument could name.
///
/// A plain word is one. A `--flag=value` is its value, and a long flag with no
/// value is read as a path too, which costs nothing when it is not one. A
/// short flag with a value glued on (`-f../list`, `-rf/etc/x`) could have it
/// start after any letter, so every tail is one; a harmless flag read as an
/// absolute path costs a prompt.
fn candidates(text: &str) -> Box<dyn Iterator<Item = &str> + '_> {
    if !text.starts_with('-') || text == "-" {
        return Box::new(std::iter::once(text));
    }
    if text.starts_with("--") {
        let value = text.split_once('=').map_or(text, |(_, v)| v);
        return Box::new(std::iter::once(value));
    }
    Box::new(
        (2..text.len())
            .filter_map(move |at| text.get(at..))
            .filter(|tail| !tail.is_empty()),
    )
}

#[cfg(test)]
mod tests;
