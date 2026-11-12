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
//!
//! What it finds outside is one of two kinds. A plain word that names one
//! exact path (`test -x /abs/tool`) is something a person can approve by
//! name, so it is reported in [`Outside::paths`] and a grant can cover that
//! program with that path and no other. Everything else it finds (an
//! expansion, a glob that reaches out, a path in an option or a redirect) is
//! in [`Outside::blocked`], and a line with any of those is approved once or
//! not at all.

use std::path::{Path, PathBuf};

use leviath_core::interaction::{GrantBlocker, GrantBlockerReason, outside_path_key};

use super::loops::Loops;
use super::options::Spelling;
use super::readings::{Reading, candidates, readings};
use super::{
    BACKSLASH_ESCAPES, Segment, Stripped, Word, WriteTarget, classify_write, strip_to_program,
};

/// What in a line reaches, or might reach, outside the workdir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Outside {
    /// The operands that name somewhere outside, or that this cannot place,
    /// as written. Any of them keeps the line off the safe list, and a line
    /// with one runs unprompted only when a person granted exactly what it
    /// names (see [`Outside::grant_keys`]).
    pub operands: Vec<String>,
    /// Why what the line does cannot be judged from its words at all, when
    /// that is so: it does not parse, or it hands a command to another
    /// interpreter (`sh -c`, `eval`, `xargs`, `python -c`). Such a line is
    /// refused where `outside_workdir = "deny"`.
    pub unplaceable: Option<String>,
    /// The outside paths a grant can name: each one exact path, handed as a
    /// plain word to one program.
    pub paths: Vec<OutsidePath>,
    /// What keeps the line from being granted for a stage or a run: each word
    /// that cannot be keyed, and why. Empty for a line that can be.
    pub blocked: Vec<GrantBlocker>,
}

/// One path outside the workdir that a line hands to a program as a plain
/// word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutsidePath {
    /// The program handed it, as the line names it.
    pub program: String,
    /// The path the shell would open: absolute, with every symlink and `..`
    /// in it resolved.
    pub path: String,
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

    /// The keys a scoped approval of the line records: `keys`, which name
    /// what it runs, then one for each outside path it names. Empty when
    /// something in the line cannot be keyed, because a grant that named the
    /// rest would go on to cover a line nobody saw.
    pub(crate) fn grant_keys(&self, mut keys: Vec<String>) -> Vec<String> {
        if keys.is_empty() || !self.blocked.is_empty() {
            return Vec::new();
        }
        for outside in &self.paths {
            let key = outside_path_key(&outside.program, &outside.path);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }
}

/// Programs that open no file named in their arguments, so a literal word is
/// not a path operand. `echo /etc/hosts` prints a string. An argument that is
/// a plain parameter expansion (`$HOME`, `${x:-y}`: see
/// [`expansion`](super::expansion)) is let through too: the program prints
/// what the variable holds and opens nothing. A substitution is not, since it
/// runs a command or reads a file: `echo "$(< /etc/hosts)"`.
///
/// Short on purpose, and the same on every platform: a program belongs here
/// only when no option of it opens a file either. `type` is not here: on
/// `cmd.exe` it prints a file. Neither are `pgrep` (`-F pidfile`), `hostname`
/// (`-F file`) or `ps` (a core file, on the BSDs).
const NO_FILE_OPERANDS: &[&str] = &[
    "echo", "printf", "seq", "sleep", "true", "false", "pwd", "basename", "dirname", "uname",
    "whoami", "id", "which", "tr",
];

/// The three spellings of `grep`.
const GREPS: [&str; 3] = ["grep", "egrep", "fgrep"];

/// How an argument makes a program read files the line does not name.
enum Unnamed {
    /// An option, however the word spells it: see [`Spelling`].
    Option(Spelling),
    /// A word that starts this way.
    Starts(&'static str),
    /// A word that holds this text.
    Holds(&'static str),
}

/// Arguments that make a program on the default safe list read files the
/// line does not name, so the read fence has nothing to check: a recursion
/// that follows every symlink it meets, a file that lists the files to read,
/// a search path inside a `jq` program. A line carrying one is not covered.
/// `diff` handed a directory is the same case, and is checked where the
/// directory can be looked for: see `Walk::is_directory`.
const READS_UNNAMED: &[(&[&str], Unnamed)] = &[
    (&GREPS, Unnamed::Option(Spelling::Letter('R'))),
    (&GREPS, Unnamed::Option(Spelling::Letter('S'))),
    (
        &GREPS,
        Unnamed::Option(Spelling::Long("--dereference-recursive")),
    ),
    (&["diff"], Unnamed::Option(Spelling::Letter('r'))),
    (&["diff"], Unnamed::Option(Spelling::Long("--recursive"))),
    (
        &["wc", "du"],
        Unnamed::Option(Spelling::Long("--files0-from")),
    ),
    (&["file"], Unnamed::Option(Spelling::Letter('f'))),
    (&["file"], Unnamed::Option(Spelling::Long("--files-from"))),
    (&["strings"], Unnamed::Starts("@")),
    (&["jq"], Unnamed::Holds("import")),
    (&["jq"], Unnamed::Holds("include")),
    (&["jq"], Unnamed::Holds("modulemeta")),
];

/// Whether handing `value` to `name` makes it read files the line does not
/// name: see [`READS_UNNAMED`].
fn reads_unnamed(name: &str, value: &str) -> bool {
    READS_UNNAMED.iter().any(|(programs, how)| {
        programs.contains(&name)
            && match how {
                Unnamed::Option(option) => option.is(value),
                Unnamed::Starts(start) => value.starts_with(start),
                Unnamed::Holds(text) => value.contains(text),
            }
    })
}

/// Whether zsh replaces the word with the path of a command: a word that
/// starts with an unquoted `=` is `=name`, the file `name` is found at on
/// `PATH`. `value` is what the word comes to once a loop variable in it is
/// read.
fn names_a_command(word: &Word, value: &str) -> bool {
    word.bare_lead && value.len() > 1 && value.starts_with('=')
}

/// Programs with an option that assigns a variable (`printf -v NAME`), which
/// an expansion standing where an option could must not be able to spell. One
/// of these takes an expansion only after its first plain operand, where the
/// program reads no more options.
const OPTION_ASSIGNS: &[&str] = &["printf"];

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
        loops: Loops::default(),
        keyed: false,
        looped: false,
        out: Outside::default(),
    };
    match super::tokenize_for(command, backslash_escapes) {
        Some(segments) => {
            walk.loops = Loops::read(command, &segments, backslash_escapes);
            for (at, segment) in segments.iter().enumerate() {
                walk.segment(at, segment);
            }
            // A line with nothing to key is one no grant speaks for.
            if !walk.keyed && walk.out.blocked.is_empty() {
                walk.out.blocked.push(GrantBlocker {
                    word: String::new(),
                    reason: GrantBlockerReason::NothingRuns,
                    detail: "nothing in the line runs a program".to_string(),
                });
            }
        }
        None => {
            walk.out.unplaceable = Some("it cannot be read as commands".to_string());
            walk.out.blocked.push(GrantBlocker {
                word: String::new(),
                reason: GrantBlockerReason::UnreadableLine,
                detail: "the line cannot be read as a list of commands".to_string(),
            });
        }
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
    /// The line's `for` loops whose variables the line spells out.
    loops: Loops,
    /// Whether anything in the line yields a key: a program, a variable it
    /// binds, a file it writes.
    keyed: bool,
    /// Whether a loop has started. A `cd` in one moves the shell for every
    /// pass after it, so the commands before it run somewhere else too.
    looped: bool,
    out: Outside,
}

/// Where a word, read as a path, points.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placed {
    /// Somewhere `may_read` allows, from every directory the shell may be in.
    Inside,
    /// Outside, at these exact paths: one per directory the shell may be in
    /// that it is outside from.
    Outside(Vec<String>),
    /// Somewhere this cannot name: a brace list, another user's home, a glob
    /// that reaches outside or is too wide to check, a path that cannot be
    /// resolved.
    Nowhere,
}

impl Walk<'_> {
    fn segment(&mut self, at: usize, segment: &Segment) {
        // A redirect's file is read whatever the segment runs, a group or a
        // loop included: `{ cat; } < /etc/hosts`.
        for word in &segment.reads {
            self.check_redirect(word, at);
        }
        for target in &segment.writes {
            self.note_write(target);
        }
        let opens_a_loop =
            |w: &Word| matches!(w.text.as_str(), "for" | "select" | "while" | "until");
        self.looped |= segment
            .words
            .iter()
            .take_while(|w| {
                w.is_bare()
                    && (super::PREFIX_KEYWORDS.contains(&w.text.as_str()) || opens_a_loop(w))
            })
            .any(opens_a_loop);
        let stripped = strip_to_program(&segment.words);
        // `cat x(id|ps)` is one word to zsh, a glob over file names, and three
        // commands to this reader; the other shells refuse it. A parenthesis
        // after an assignment (`a=(1 2)`) or a keyword (`if (test -e x)`) is
        // neither a program's word nor a redirect's.
        let before = match (&stripped, segment.reads.last().or(segment.writes.last())) {
            (Stripped::Program { words, .. }, _) => words.last(),
            (_, redirect) => redirect,
        };
        if let Some(word) = before.filter(|_| segment.grouped) {
            self.block(
                format!("{}( (a glob in zsh)", word.shown),
                &word.shown,
                GrantBlockerReason::UnplaceablePath,
                "comes before a parenthesis, which zsh reads as a glob over file names",
            );
        }
        let words = match stripped {
            Stripped::Program { words, .. } => words,
            Stripped::Unreadable(why) => {
                self.out.unplaceable = Some("it runs a command assembled at run time".to_string());
                self.out.blocked.push(why);
                return;
            }
            Stripped::NothingRuns(keys) => {
                self.keyed |= !keys.is_empty();
                return;
            }
        };
        self.keyed = true;
        let program = &words[0];
        if self.left {
            self.block(
                "(after a cd out of the working directory)".to_string(),
                &program.shown,
                GrantBlockerReason::UnplaceablePath,
                "runs after a `cd` out of the working directory",
            );
        }
        let name = program_name(&program.text);
        if is_runner(name, &words[1..]) {
            self.out.unplaceable = Some(format!("`{name}` runs a command this cannot read"));
        }
        if MOVES_THE_SHELL.contains(&name) {
            self.move_shell(program, &words[1..], at);
            return;
        }
        let opens_files = !NO_FILE_OPERANDS.contains(&name);
        // Whether an expansion here could still be read as an option that
        // assigns a variable.
        let mut may_assign = OPTION_ASSIGNS.contains(&name);
        let mut options_ended = false;
        for word in &words[1..] {
            let positional = options_ended;
            options_ended |= word.text == "--";
            let Some(values) = self.values(word, at) else {
                // A program that opens no file may print what a variable
                // holds; anything else handed an expansion may open it.
                let printed =
                    !opens_files && !may_assign && word.expands.is_plain() && !self.expands(word);
                if !printed {
                    self.block_expanded(word);
                }
                continue;
            };
            for value in &values {
                may_assign &= value.starts_with('-');
                if reads_unnamed(name, value) || (name == "diff" && self.is_directory(value)) {
                    self.block(
                        format!("{name} {value} (reads files the line does not name)"),
                        &word.shown,
                        GrantBlockerReason::UnplaceablePath,
                        "makes the program read files the line does not name",
                    );
                }
                if opens_files {
                    self.check_arg(program, word, value, positional);
                }
            }
        }
    }

    /// Every value `word` can have, when the line itself says: its text for a
    /// literal word, the values of the loop variables it names for a word in
    /// a loop that spells them out (see [`Loops`]). `None` for any other
    /// word, which only the shell can expand.
    fn values(&self, word: &Word, at: usize) -> Option<Vec<String>> {
        match (word.literal, self.expands(word)) {
            (_, true) => None,
            (true, false) => Some(vec![word.text.clone()]),
            (false, false) => self.loops.resolve(word, at),
        }
    }

    /// A `cd`: its target is a path operand, and where it lands decides what
    /// every later segment's relative paths mean.
    fn move_shell(&mut self, program: &Word, args: &[Word], at: usize) {
        if self.looped {
            return self.block(
                "cd (in a loop)".to_string(),
                &program.shown,
                GrantBlockerReason::UnplaceablePath,
                "comes after a loop has started, where it moves the shell for every pass",
            );
        }
        // No target is `$HOME`, and `-` is wherever the shell was before;
        // two targets are, in zsh, the current directory with the first
        // replaced by the second. None is a place this can vouch for.
        let mut targets = args.iter().filter(|w| !w.text.starts_with('-'));
        let (Some(target), None) = (targets.next(), targets.next()) else {
            self.left = true;
            self.block(
                "cd".to_string(),
                &program.shown,
                GrantBlockerReason::UnplaceablePath,
                "goes somewhere the line does not say: home, the last directory, or with two \
                 names zsh's rewrite of the current one",
            );
            return;
        };
        let Some(values) = self.values(target, at) else {
            self.left = true;
            self.block_expanded(target);
            return;
        };
        for value in &values {
            if names_a_command(target, value) || self.place(value) != Placed::Inside {
                self.left = true;
                self.block(
                    target.shown.clone(),
                    &target.shown,
                    GrantBlockerReason::OutsideWorkdir,
                    "takes the shell out of the working directory",
                );
                return;
            }
            // Placed inside, so the target expands and folds.
            let target = leviath_core::paths::expand_home(value, self.home).unwrap_or_default();
            let moved: Vec<PathBuf> = self
                .bases
                .iter()
                .filter_map(|base| leviath_core::paths::fold_dot_dot(&base.join(&target)))
                .collect();
            self.bases.extend(moved);
        }
    }

    /// Whether `value`, or what follows its `=`, names a directory from
    /// somewhere the shell may be. `diff` handed one compares every file in
    /// it, through any symlink, with no `-r`.
    fn is_directory(&self, value: &str) -> bool {
        let named = value.rsplit('=').next().unwrap_or(value);
        self.bases.iter().any(|base| base.join(named).is_dir())
    }

    /// Whether a word is expanded by this shell even though the lexer kept it
    /// literal: `%VAR%` on `cmd.exe`.
    fn expands(&self, word: &Word) -> bool {
        self.cmd && word.text.contains('%')
    }

    /// Record an operand that keeps the line from being granted: `operand` as
    /// the refusal names it, and `word` with why it cannot be keyed.
    fn block(&mut self, operand: String, word: &str, reason: GrantBlockerReason, why: &str) {
        self.out.operands.push(operand);
        self.out.blocked.push(GrantBlocker {
            word: word.to_string(),
            reason,
            detail: format!("`{word}` {why}"),
        });
    }

    /// Record a word only the shell can expand.
    fn block_expanded(&mut self, word: &Word) {
        let shown = match word.text.is_empty() {
            true => "a substitution",
            false => word.shown.as_str(),
        };
        self.block(
            format!("{shown} (expanded at run time)"),
            &word.shown,
            GrantBlockerReason::ExpandedAtRunTime,
            "is expanded at run time",
        );
    }

    /// Check one argument of `program`, as an option or a plain word. `word`
    /// is the argument as the line spells it and `value` what the program is
    /// handed.
    ///
    /// Every reading of it has to stay inside (see `readings`). A plain word
    /// whose readings outside are each one exact path is something a grant
    /// can name, path by path. An option's value is not, and neither is a
    /// word with a reading that cannot be placed.
    fn check_arg(&mut self, program: &Word, word: &Word, value: &str, positional: bool) {
        let shown = word.shown.as_str();
        if names_a_command(word, value) {
            return self.block(
                shown.to_string(),
                shown,
                GrantBlockerReason::UnplaceablePath,
                "is replaced by zsh with the path of a command",
            );
        }
        let option = !positional && value.starts_with('-') && value != "-";
        let carried = match option {
            true => candidates(value),
            false => vec![value],
        };
        let mut outside: Vec<String> = Vec::new();
        let mut lost = false;
        for carried in carried {
            for reading in readings(carried).unwrap_or_else(|| {
                lost = true;
                Vec::new()
            }) {
                match self.place_reading(reading) {
                    Placed::Inside => {}
                    Placed::Outside(paths) => outside.extend(paths),
                    Placed::Nowhere => lost = true,
                }
            }
        }
        match (option, lost, outside.is_empty()) {
            (_, false, true) => {}
            (true, _, _) => self.block(
                shown.to_string(),
                shown,
                GrantBlockerReason::OutsideWorkdir,
                "names a path outside the working directory inside an option",
            ),
            (false, true, _) => self.block(
                shown.to_string(),
                shown,
                GrantBlockerReason::UnplaceablePath,
                "may name something outside the working directory that cannot be placed",
            ),
            (false, false, false) => {
                self.out.operands.push(shown.to_string());
                outside.sort();
                outside.dedup();
                self.out
                    .paths
                    .extend(outside.into_iter().map(|path| OutsidePath {
                        program: program.text.clone(),
                        path,
                    }));
            }
        }
    }

    /// Where one reading of a value points. A part of a value that is an
    /// absolute path is something a program opens only by taking the value
    /// apart, so one with nothing at the root to open is not a read.
    fn place_reading(&self, reading: Reading<'_>) -> Placed {
        match reading.part && self.nothing_there(reading.text) {
            true => Placed::Inside,
            false => self.place(reading.text),
        }
    }

    /// Whether `part` is an absolute path whose first name does not exist at
    /// the root of the file system the workdir is on: `//tests` out of
    /// `res://tests`, `/api/v1` out of `path=/api/v1`. Nothing can be opened
    /// under a name that is not there. The root itself (`/` out of `a:/`)
    /// names no file either.
    fn nothing_there(&self, part: &str) -> bool {
        let Some(rest) = part.strip_prefix('/') else {
            return false;
        };
        let name = rest.trim_start_matches('/').split('/').next().unwrap_or("");
        let root = self.workdir.ancestors().last().unwrap_or(self.workdir);
        name.is_empty() || root.join(name).symlink_metadata().is_err()
    }

    /// Check the file a `<` redirect reads. The shell opens it, so it is read
    /// as it is spelled, and it is no program's argument for a grant to name.
    fn check_redirect(&mut self, word: &Word, at: usize) {
        let Some(values) = self.values(word, at) else {
            return self.block_expanded(word);
        };
        let outside = |v: &String| names_a_command(word, v) || self.place(v) != Placed::Inside;
        if values.iter().any(outside) {
            self.block(
                word.shown.clone(),
                &word.shown,
                GrantBlockerReason::OutsideWorkdir,
                "is read through a redirect from outside the working directory",
            );
        }
    }

    /// Note what a `>` redirect writes: a path is a key of its own, and a
    /// target no key can name keeps the line from being granted.
    fn note_write(&mut self, target: &Word) {
        match (classify_write(target), target.literal) {
            (WriteTarget::Discarded, _) => {}
            (WriteTarget::Path(_), _) => self.keyed = true,
            (WriteTarget::Unreadable, false) => self.out.blocked.push(GrantBlocker {
                word: target.shown.clone(),
                reason: GrantBlockerReason::ExpandedAtRunTime,
                detail: format!(
                    "`{}` is a redirect target expanded at run time",
                    target.shown
                ),
            }),
            (WriteTarget::Unreadable, true) => self.out.blocked.push(GrantBlocker {
                word: target.shown.clone(),
                reason: GrantBlockerReason::UnplaceablePath,
                detail: format!("`{}` is a redirect to a network connection", target.shown),
            }),
        }
    }

    /// Where `text`, as a path, points from every base the shell could be in.
    fn place(&self, text: &str) -> Placed {
        // A brace list is expanded by the shell into words this never sees.
        if text.contains('{') && (text.contains(',') || text.contains("..")) {
            return Placed::Nowhere;
        }
        // `~name` is somebody else's home, which nothing here looks up.
        if text.starts_with('~') && text != "~" && !text.starts_with("~/") {
            return Placed::Nowhere;
        }
        let Some(path) = leviath_core::paths::expand_home(text, self.home) else {
            return Placed::Nowhere;
        };
        // Asked of the word, before it is joined to a base: joining onto a
        // Windows verbatim path folds the `..` away.
        let climbs = path
            .components()
            .any(|c| c == std::path::Component::ParentDir);
        let globs = text.contains(['*', '?', '[']);
        let mut outside = Vec::new();
        for base in &self.bases {
            let Some(paths) = expansions(base, &path) else {
                return Placed::Nowhere;
            };
            for path in paths.iter().filter(|p| !self.reads_inside(p, climbs)) {
                // A glob that reaches outside names whatever is there when it
                // runs, which is not one path.
                let exact = (!globs).then(|| exact_path(path)).flatten();
                match exact {
                    Some(exact) if !outside.contains(&exact) => outside.push(exact),
                    Some(_) => {}
                    None => return Placed::Nowhere,
                }
            }
        }
        match outside.is_empty() {
            true => Placed::Inside,
            false => Placed::Outside(outside),
        }
    }

    /// Whether the shell, opening `path`, opens something `may_read` allows.
    ///
    /// Two checks when the word `climbs` with a `..`. The fence folds `..`
    /// lexically, but the shell resolves it physically, so `link/../secret`
    /// with `link` pointing elsewhere opens a file beside the link's target.
    /// Such a path is therefore also canonicalized as written and held to the
    /// workdir. (On Windows the join has already folded it, so this is the
    /// lexical answer again; a symlink there needs a privilege to make.)
    fn reads_inside(&self, path: &Path, climbs: bool) -> bool {
        (self.may_read)(path) && (!climbs || leviath_core::resolves_within(path, self.workdir))
    }
}

/// The one path the shell opens for `path`, as a grant names it: every
/// symlink and `..` resolved the way the kernel resolves them, with no
/// trailing separator. `None` when that cannot be said: a `..` after a name
/// that does not exist yet, a dangling symlink, a name that is not UTF-8.
///
/// Two spellings of one file give one path, so a grant for it covers both;
/// and a symlink that is repointed gives a different path, so a grant for
/// what it pointed at does not follow it. Nothing folds case: on a file
/// system that ignores it the resolved path already carries the name's own
/// spelling, and on one that does not two cases are two files.
fn exact_path(path: &Path) -> Option<String> {
    leviath_core::canonicalize_for_match(path).and_then(|real| real.to_str().map(str::to_string))
}

/// The paths a word names once the shell has expanded its glob, or `None`
/// when that cannot be checked: a pattern that could match `..`, which a
/// directory listing never shows but the shell does, zsh's `***`, which
/// walks through every symlink it meets, or more matches than
/// [`MAX_GLOB_MATCHES`]. The word as written is always one of the paths: the
/// shell passes on a glob that matches nothing, and a `*` or a `[x]` written
/// between quotes was a name all along.
pub(super) fn expansions(base: &Path, word: &Path) -> Option<Vec<PathBuf>> {
    let path = base.join(word);
    let text = word.to_string_lossy();
    if !text.contains(['*', '?', '[']) {
        return Some(vec![path]);
    }
    if text.contains("***") {
        return None;
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
    let mut found: Vec<PathBuf> = glob::glob_with(&pattern, dots)
        .into_iter()
        .flatten()
        .flatten()
        .take(MAX_GLOB_MATCHES + 1)
        .map(|p| PathBuf::from(format!("{prefix}{}", p.display())))
        .collect();
    if found.len() > MAX_GLOB_MATCHES {
        return None;
    }
    found.push(path);
    Some(found)
}

/// The prefix a Windows verbatim path carries.
const VERBATIM: &str = r"\\?\";

/// A path's Windows verbatim prefix and the rest of it, or no prefix.
fn split_verbatim(path: &str) -> (&str, &str) {
    path.strip_prefix(VERBATIM)
        .map_or(("", path), |rest| (VERBATIM, rest))
}

/// The program's own name, without the directory it was named from.
pub(super) fn program_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// Whether `name` with `args` runs a command or a script this cannot read.
fn is_runner(name: &str, args: &[Word]) -> bool {
    RUNNERS.contains(&name)
        || name.starts_with("python")
        // `git -c alias.x='!cmd' x` runs a shell command.
        || (name == "git" && args.iter().any(|w| w.text.starts_with("-c")))
}

#[cfg(test)]
mod tests;
