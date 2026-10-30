//! Where a line's writes land, as paths, for a fence that has to say where
//! every write goes.
//!
//! A line writes through a redirect (`> a`) and through the programs that
//! write the files their arguments name: `rm`, `mkdir`, `cp`, `mv`, `tee` and
//! the rest of [`WRITERS`]. [`write_targets_for`](super::write_targets_for)
//! names the redirect targets as the line spells them. A fence needs more: a
//! relative target lands under whatever directory the line has moved to by
//! then, so `cd src && echo x > docs/a` writes `src/docs/a` and `cd /tmp &&
//! rm -rf x` removes `/tmp/x`.
//!
//! Only the plainest move is followed: a segment that is exactly `cd <dir>`
//! or `pushd <dir>` with a literal directory, in any position of a line
//! joined by `&&`, `||`, `;` or `|`. Such a move may fail, so the directory
//! before it stays possible too, and a target is resolved against every
//! directory the line may be in. Anything else on the line that can change
//! directory (a `cd` behind `{`, `!`, `then`, `command` or an assignment,
//! `popd`, `chdir`, `CDPATH`, `.`, `source`, `eval`, or a program that is
//! itself an expansion) leaves every relative target [`Landing::Lost`], and a
//! target the shell expands into places this cannot list (a brace list that
//! climbs, `~user`, a glob with too many matches) is lost too. A target that
//! only the shell can name (`$OUT`, `$(pwd)/x`) is [`Landing::Unnamed`]. The
//! fence treats both as outside: a write it cannot place is not one it may
//! let through. An absolute target lands where it says whatever the line did
//! first.
//!
//! What this does not see is a write hidden inside another program: `sh -c
//! "rm -rf /tmp/x"`, `xargs rm`, `find -delete`, `python -c ...`. Those are
//! the read fence's unplaceable lines and the sandbox's business.

use std::path::{Path, PathBuf};

use super::operands::{expansions, program_name};
use super::{Segment, Stripped, Word, WriteTarget, classify_write, strip_to_program, tokenize_for};

/// The most directories a line is followed into before it counts as lost.
const MAX_CWDS: usize = 64;

/// Words that change the shell's directory, anywhere in a segment. The
/// programs that run text which may (`.`, `source`, `eval`) are not here: as
/// programs they are unreadable (see [`program_unknown`]), and as arguments
/// (`git add .`) they move nothing.
const MOVERS: &[&str] = &["cd", "pushd", "popd", "chdir"];

/// Words in front of a program that run it as named: `command rm x` runs
/// `rm`. The same three [`program_unknown`] looks past.
const WRAPPERS: &[&str] = &["command", "builtin", "exec"];

/// Which of a writing program's arguments name what it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operands {
    /// Every path operand: `rm a b` removes both, `mv a b` removes `a` and
    /// makes `b`.
    Every,
    /// The last path operand, or the `-t`/`--target-directory` value when
    /// there is one: `cp a b c` reads `a` and `b` and writes `c`.
    Destination,
    /// Every `of=<file>`: `dd`.
    OfEquals,
    /// `git`: the `-C`, `--git-dir` and `--work-tree` values when the
    /// subcommand is not in [`GIT_READ_ONLY`], and every path operand of a
    /// subcommand in [`GIT_MAKES_AT`].
    Git,
}

/// Programs that write the files their arguments name: the program, which
/// arguments it writes, and the options that take a value of their own, so
/// the value is not read as an operand (`mkdir -m 755 dir` writes `dir`, not
/// `755`). A `-t`/`--target-directory` value is always a target.
///
/// A table rather than a match, so adding a program is one row. The programs
/// here are the ones a coding agent reaches for; a write through a program
/// not listed is the sandbox's to stop.
const WRITERS: &[(&str, Operands, &[&str])] = &[
    ("rm", Operands::Every, &[]),
    ("rmdir", Operands::Every, &[]),
    ("mkdir", Operands::Every, &["-m", "--mode"]),
    (
        "touch",
        Operands::Every,
        &["-d", "-r", "-t", "--date", "--reference", "--time"],
    ),
    (
        "truncate",
        Operands::Every,
        &["-s", "-r", "--size", "--reference"],
    ),
    ("tee", Operands::Every, &[]),
    (
        "mv",
        Operands::Every,
        &["-S", "-t", "--suffix", "--target-directory"],
    ),
    (
        "cp",
        Operands::Destination,
        &["-S", "-t", "--suffix", "--target-directory"],
    ),
    (
        "ln",
        Operands::Destination,
        &["-S", "-t", "--suffix", "--target-directory"],
    ),
    (
        "install",
        Operands::Destination,
        &[
            "-g",
            "-m",
            "-o",
            "-S",
            "-t",
            "--group",
            "--mode",
            "--owner",
            "--suffix",
            "--target-directory",
        ],
    ),
    ("dd", Operands::OfEquals, &[]),
    (
        "git",
        Operands::Git,
        &[
            "-C",
            "-c",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--exec-path",
            "-b",
            "-B",
            "-o",
            "--branch",
            "--origin",
            "--depth",
            "--reference",
            "--template",
            "--separate-git-dir",
        ],
    ),
];

/// Options whose value names where a program writes, whatever its operands.
const TARGET_OPTIONS: &[&str] = &["-t", "--target-directory"];

/// `git`'s global options whose value is the repository a writing subcommand
/// writes to.
const GIT_REPO_OPTIONS: &[&str] = &["-C", "--git-dir", "--work-tree"];

/// `git` subcommands that only read the repository, so `git -C <dir>` with
/// one of them writes nothing there. Anything not listed is taken to write:
/// `branch`, `tag`, `config` and `remote` all read with no arguments and
/// write with some, and a wrong guess here costs a refusal of a read, not a
/// write nobody judged.
const GIT_READ_ONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "blame",
    "cat-file",
    "describe",
    "shortlog",
    "grep",
    "rev-list",
    "name-rev",
    "for-each-ref",
    "reflog",
    "version",
    "help",
    "check-ignore",
    "merge-base",
    "show-ref",
    "count-objects",
    "var",
    "whatchanged",
    "diff-tree",
    "diff-index",
    "diff-files",
];

/// `git` subcommands whose path operands name where a repository is made.
///
/// With the number of verbs before the path: `git worktree add <path>`.
const GIT_MAKES_AT: &[(&str, usize)] = &[("init", 0), ("clone", 0), ("worktree", 1)];

/// One write target: as the line spells it, every path it may land on, and
/// what writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The target as written.
    pub(crate) written: String,
    /// Every path it may land on, from each directory the line may be in.
    pub(crate) lands: Vec<PathBuf>,
    /// What writes it: `redirect` for a `>`, else the program's name.
    pub(crate) by: String,
}

/// What a redirect target is written by.
pub(crate) const REDIRECT: &str = "redirect";

/// Where a line's writes may land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landing {
    /// Every target, with where it may land.
    Known(Vec<Target>),
    /// The line does not parse, and has a `>` in it.
    Unparsed,
    /// A target is not literal: `> $OUT`, `>& $fd`, `rm -rf $DIR`,
    /// `/dev/tcp/...`.
    Unnamed,
    /// A relative target on a line that may change directory in a way this
    /// does not follow, or a target the shell expands into places this cannot
    /// list.
    Lost,
}

/// Where `command`'s writes may land, run from `workdir`. With
/// `backslash_escapes` (a POSIX shell), `> NUL` is a file named `NUL` like
/// any other; only `cmd.exe` discards it.
pub(crate) fn landing_for(
    command: &str,
    backslash_escapes: bool,
    workdir: &Path,
    home: Option<&Path>,
) -> Landing {
    let Some(segments) = tokenize_for(command, backslash_escapes) else {
        return match command.contains('>') {
            true => Landing::Unparsed,
            false => Landing::Known(Vec::new()),
        };
    };
    let mut place = Placer {
        followed: segments
            .iter()
            .all(|s| !moves(s) || plain_move(s, home).is_some()),
        cwds: vec![workdir.to_path_buf()],
        home,
    };
    let mut out = Vec::new();
    for segment in &segments {
        for target in &segment.writes {
            let written = match classify_write(target) {
                WriteTarget::Path(text) => text,
                WriteTarget::Discarded
                    if backslash_escapes && target.text.eq_ignore_ascii_case("NUL") =>
                {
                    target.text.clone()
                }
                WriteTarget::Discarded => continue,
                WriteTarget::Unreadable => return Landing::Unnamed,
            };
            match place.place(&written, true, REDIRECT) {
                Ok(target) => out.push(target),
                Err(landing) => return landing,
            }
        }
        if let Some((by, operands)) = program_writes(&segment.words) {
            for operand in operands {
                match place.place(operand.text, operand.literal, by) {
                    Ok(target) => out.push(target),
                    Err(landing) => return landing,
                }
            }
        }
        match plain_move(segment, home) {
            // Each move doubles the directories the line may be in; past a
            // handful of them the line is no longer followed.
            Some(next) if place.cwds.len() < MAX_CWDS => {
                let more: Vec<PathBuf> = place.cwds.iter().map(|cwd| cwd.join(&next)).collect();
                place.cwds.extend(more);
            }
            Some(_) => place.followed = false,
            None => {}
        }
    }
    Landing::Known(out)
}

/// Where one target lands, from every directory a line may be in.
struct Placer<'a> {
    /// Whether every move on the line is one this follows.
    followed: bool,
    /// Every directory the line may be in by now.
    cwds: Vec<PathBuf>,
    home: Option<&'a Path>,
}

impl Placer<'_> {
    /// Every path `written` may land on, or why it cannot be placed: not
    /// `literal` (`$DIR`), a home only the shell knows (`~user`), a brace
    /// list that could climb, a relative target after a move not followed,
    /// or a glob that could match `..` or too much.
    fn place(&self, written: &str, literal: bool, by: &str) -> Result<Target, Landing> {
        if !literal {
            return Err(Landing::Unnamed);
        }
        let home_named = written == "~" || written.starts_with("~/");
        if (written.starts_with('~') && !home_named) || brace_escapes(written) {
            return Err(Landing::Lost);
        }
        let path = leviath_core::paths::expand_home(written, self.home).ok_or(Landing::Lost)?;
        let bases = match (path.is_absolute(), self.followed) {
            (true, _) => &self.cwds[..1],
            (false, true) => self.cwds.as_slice(),
            (false, false) => return Err(Landing::Lost),
        };
        let mut lands = Vec::new();
        for base in bases {
            lands.extend(expansions(base, &path).ok_or(Landing::Lost)?);
        }
        Ok(Target {
            written: written.to_string(),
            lands,
            by: by.to_string(),
        })
    }
}

/// Whether a brace list could expand to somewhere other than under the word's
/// own prefix: `{a,b}` stays put, `{..,x}/y`, `{/tmp,x}` and `{~,x}` do not.
fn brace_escapes(text: &str) -> bool {
    let brace = text.contains('{') && (text.contains(',') || text.contains(".."));
    brace
        && (text.contains("..") || text.contains("{/") || text.contains(",/") || text.contains('~'))
}

/// Whether `segment` may change the shell's directory, or run something that
/// might: a mover anywhere in it, `CDPATH` anywhere in it, or a program this
/// cannot name (see [`program_unknown`]).
fn moves(segment: &Segment) -> bool {
    segment
        .words
        .iter()
        .any(|w| MOVERS.contains(&w.text.as_str()) || w.text.contains("CDPATH"))
        || program_unknown(&segment.words)
}

/// Whether the program `words` run is one this cannot name: past keywords,
/// bindings and `command`, `builtin` or `exec`, an expansion
/// (`{ $X src; }`, `$(printf c)d`), or a program that runs text it was handed.
fn program_unknown(words: &[Word]) -> bool {
    match strip_to_program(words) {
        Stripped::Unreadable => true,
        Stripped::NothingRuns(_) => false,
        Stripped::Program { words, .. } => match words {
            [prefix, rest @ ..] if WRAPPERS.contains(&prefix.text.as_str()) => {
                program_unknown(rest)
            }
            _ => false,
        },
    }
}

/// The directory `segment` moves to when it is exactly `cd <dir>` or
/// `pushd <dir>`, `<dir>` literal and not an option or `-`.
fn plain_move(segment: &Segment, home: Option<&Path>) -> Option<PathBuf> {
    let [program, target] = segment.words.as_slice() else {
        return None;
    };
    // `~-`, `~+` and `~user` name directories only the shell knows; `~` and
    // `~/...` are the home directory, which is known.
    let tilde = target.text.starts_with('~');
    let home_named = target.text == "~" || target.text.starts_with("~/");
    let plain = matches!(program.text.as_str(), "cd" | "pushd")
        && program.literal
        && target.literal
        && !target.text.starts_with('-')
        && (!tilde || home_named);
    plain
        .then(|| leviath_core::paths::expand_home(&target.text, home))
        .flatten()
}

/// The program `words` run, past keywords, bindings and wrappers, with its
/// arguments: `FOO=1 command /bin/rm -rf x` is `rm` with `-rf x`. `None` when
/// nothing runs or what runs cannot be named.
fn program_of(words: &[Word]) -> Option<(&str, &[Word])> {
    let Stripped::Program { mut words, .. } = strip_to_program(words) else {
        return None;
    };
    while let [prefix, rest @ ..] = words
        && WRAPPERS.contains(&prefix.text.as_str())
    {
        words = rest;
    }
    let [program, args @ ..] = words else {
        return None;
    };
    Some((program_name(&program.text), args))
}

/// One argument, or the part of one, that may name a path.
#[derive(Debug, Clone, Copy)]
struct Operand<'a> {
    text: &'a str,
    /// Whether the text is fully determined by the source: see [`Word`].
    literal: bool,
}

impl<'a> Operand<'a> {
    fn of(word: &'a Word) -> Self {
        Self {
            text: &word.text,
            literal: word.literal,
        }
    }
}

/// The program `words` run and the arguments naming what it writes, per
/// [`WRITERS`]. `None` for a program that writes nothing a path names.
fn program_writes(words: &[Word]) -> Option<(&str, Vec<Operand<'_>>)> {
    let (name, args) = program_of(words)?;
    let (_, operands, valued) = WRITERS.iter().find(|(p, ..)| *p == name)?;
    let (positionals, from_options) = split_args(args, valued, TARGET_OPTIONS);
    let written = match operands {
        Operands::Every => positionals.into_iter().chain(from_options).collect(),
        Operands::Destination => match from_options.is_empty() {
            true => positionals.last().copied().into_iter().collect(),
            false => from_options,
        },
        // A word that expands may be an `of=` once it has: fail closed.
        Operands::OfEquals => args
            .iter()
            .filter(|w| w.text.starts_with("of=") || !w.literal)
            .map(|w| Operand {
                text: w.text.strip_prefix("of=").unwrap_or(&w.text),
                literal: w.literal,
            })
            .collect(),
        Operands::Git => git_operands(args, valued),
    };
    Some((name, written))
}

/// `git`'s write targets: the repository its global options name when the
/// subcommand writes, and the path operands of a subcommand that makes a
/// repository somewhere.
fn git_operands<'a>(args: &'a [Word], valued: &'static [&'static str]) -> Vec<Operand<'a>> {
    let (positionals, repo) = split_args(args, valued, GIT_REPO_OPTIONS);
    let Some((subcommand, rest)) = positionals.split_first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if !GIT_READ_ONLY.contains(&subcommand.text) {
        out.extend(repo);
    }
    if let Some((_, verbs)) = GIT_MAKES_AT.iter().find(|(s, _)| *s == subcommand.text) {
        out.extend(rest.iter().skip(*verbs).copied());
    }
    out
}

/// `args` split into its positionals and the values of the options in
/// `targets`, with the values of every option in `valued` taken off the
/// positionals. A value sits in the next word, after `=` on a long option, or
/// glued to a short one (`-t/tmp`, `-rt /tmp`); after `--` every word is a
/// positional.
fn split_args<'a>(
    args: &'a [Word],
    valued: &'static [&'static str],
    targets: &[&str],
) -> (Vec<Operand<'a>>, Vec<Operand<'a>>) {
    let mut positionals = Vec::new();
    let mut from_targets = Vec::new();
    let mut options_ended = false;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let text = word.text.as_str();
        if options_ended || !text.starts_with('-') || text == "-" {
            positionals.push(Operand::of(word));
            continue;
        }
        if text == "--" {
            options_ended = true;
            continue;
        }
        let Some((option, glued)) = valued_option(text, valued) else {
            continue;
        };
        // An option word that expands (`-t$OUT`) has a value only the shell
        // knows, wherever its text ends.
        let value = match (glued, word.literal) {
            (_, false) => Some(Operand {
                text: "",
                literal: false,
            }),
            (Some(text), true) => Some(Operand {
                text,
                literal: true,
            }),
            (None, true) => words.next().map(Operand::of),
        };
        if let Some(value) = value
            && targets.contains(&option)
        {
            from_targets.push(value);
        }
    }
    (positionals, from_targets)
}

/// The option in `valued` that `text` spells, and the value glued to it when
/// one is: `--mode=755` is `--mode` with `755`, `-m755` is `-m` with `755`,
/// `-pm` is `-m` with nothing (its value is the next word). `None` for an
/// option that takes no value.
fn valued_option<'a>(
    text: &'a str,
    valued: &'static [&'static str],
) -> Option<(&'static str, Option<&'a str>)> {
    if let Some(long) = text.strip_prefix("--") {
        let (name, value) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        let option = valued.iter().find(|o| o.strip_prefix("--") == Some(name))?;
        return Some((option, value));
    }
    // In a cluster of short options the first that takes a value takes the
    // rest of the word, or the next word when nothing follows it.
    // Called for a word that starts with `-`; the fallbacks are never taken.
    let letters = text.strip_prefix('-').unwrap_or(text);
    letters.char_indices().find_map(|(at, letter)| {
        let option = valued
            .iter()
            .find(|o| o.len() == 2 && o.ends_with(letter))?;
        let glued = letters.get(at + letter.len_utf8()..).unwrap_or_default();
        Some((*option, (!glued.is_empty()).then_some(glued)))
    })
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
