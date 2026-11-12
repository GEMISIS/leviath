//! What a `for` loop's variable can be, when the line itself says.
//!
//! `for d in a b; do test -e "$d"; done` hands `test` a word the shell
//! expands at run time, and an expanded word is one the read fence cannot
//! place. Here the line spells out every value the variable takes, so the
//! fence can check `test -e a` and `test -e b` instead. That is a reading of
//! what the shell will do, and it holds only where nothing else on the line
//! can make it false:
//!
//! - **The list is spelled out.** Every word after `in` is literal and made
//!   of letters, digits and `_-./:,@%+=`, and does not start with `-` or `=`.
//!   A glob, a brace list, a `~`, an expansion or a substitution in the list
//!   leaves the loop unread, as does an empty list.
//! - **The variable is an ordinary one.** A lower-case name that no shell
//!   sets on its own: `$_`, `$PWD`, `$REPLY`, zsh's `$argv` and `$path` and
//!   their kin change underneath a loop, so a loop over one of them says
//!   nothing about what it holds.
//! - **Only inside the body.** Between the loop's `do` and its `done` the
//!   variable holds one of the listed values. After `done` it holds the last
//!   one only if the loop ran, and `false && for ...`, a loop in a pipeline
//!   and a loop in a subshell all leave it as it was.
//! - **Nothing else on the line names the variable.** An assignment, a
//!   `read`, an `unset`, a nested loop over the same name, a `printf -vNAME`:
//!   any word that holds the name other than as `$NAME` or `${NAME}` leaves
//!   every loop over it unread. A flag spelled as the name alone (`-d` for a
//!   loop over `d`) is the one exception.
//! - **The line nests the way it reads.** This reader finds a loop's body by
//!   its keywords, and a keyword is only one where the shell would read it
//!   as one. A parenthesis, a heredoc, `[[`, a comment, a line continuation
//!   or an expansion that is not a plain one can put a `done` where the shell
//!   sees none, so a line holding any of them is not read for loops at all.
//! - **Not in an option.** A word that starts with `-` is not read:
//!   `grep -$d` with `d` set to `R` is a flag, and a flag changes what the
//!   program does with the words after it.
//! - **Unquoted, only with the default `IFS`.** `$d` outside quotes is split
//!   on `IFS` by `bash` and `sh`, and a value with none of the default
//!   separators in it can still be cut by another: a line that mentions `IFS`
//!   has only its quoted uses read.

use super::{Expands, Segment, Word, is_variable_name};

/// The most spellings one word is resolved into: a word naming several loop
/// variables multiplies them.
const MAX_SPELLINGS: usize = 64;

/// Lower-case parameters `zsh` sets or ties on its own (`bash` and `sh` have
/// none but `_`, which is not lower-case by the rule here). A loop over one
/// does not decide what it holds: `argv` is the positional parameters, `path`
/// is `PATH`, `status` and `pipestatus` change with every command.
const SHELL_SET: &[&str] = &[
    "aliases",
    "argv",
    "builtins",
    "cdpath",
    "commands",
    "dirstack",
    "dis_aliases",
    "dis_builtins",
    "dis_functions",
    "dis_functions_source",
    "dis_galiases",
    "dis_patchars",
    "dis_reswords",
    "dis_saliases",
    "epochtime",
    "errnos",
    "fignore",
    "fpath",
    "funcfiletrace",
    "funcsourcetrace",
    "funcstack",
    "functions",
    "functions_source",
    "functrace",
    "galiases",
    "histchars",
    "history",
    "historywords",
    "jobdirs",
    "jobstates",
    "jobtexts",
    "keymaps",
    "langinfo",
    "mailpath",
    "manpath",
    "mapfile",
    "match",
    "mbegin",
    "mend",
    "module_path",
    "modules",
    "nameddirs",
    "options",
    "parameters",
    "patchars",
    "path",
    "pipestatus",
    "prompt",
    "psvar",
    "reply",
    "reswords",
    "saliases",
    "signals",
    "status",
    "sysparams",
    "termcap",
    "terminfo",
    "userdirs",
    "usergroups",
    "watch",
    "widgets",
    "zcurses_attrs",
    "zcurses_colors",
    "zcurses_keycodes",
    "zcurses_windows",
    "zle_bracketed_paste",
    "zsh_eval_context",
    "zsh_scheduled_events",
];

/// One `for` or `select` loop of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Loop {
    /// The variable it binds, when its header names one this can read.
    name: Option<String>,
    /// Every value the variable takes, when the header spells them out.
    values: Option<Vec<String>>,
    /// The segment its body starts at: the one after its header.
    start: usize,
    /// The segment its `done` is in, once that has been seen.
    end: Option<usize>,
}

/// The loops of one line whose variables can be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Loops {
    loops: Vec<Loop>,
    /// Whether the line mentions `IFS`, so an unquoted use may be split on
    /// something other than the default.
    splits: bool,
}

impl Loops {
    /// Read the loops of `segments`, which are `command` as tokenized for a
    /// POSIX shell when `posix` is set. A line that may not nest the way it
    /// reads has none.
    pub(super) fn read(command: &str, segments: &[Segment], posix: bool) -> Self {
        let words = || {
            segments
                .iter()
                .flat_map(|s| s.words.iter().chain(&s.reads).chain(&s.writes))
        };
        let nests_as_read = posix
            && !command.contains("\\\n")
            && !command.contains("\\\r")
            && !segments.iter().any(|s| s.tangled)
            && !words().any(|w| w.shown.starts_with('#') || (w.is_bare() && w.text == "[["));
        if !nests_as_read {
            return Self::default();
        }
        let mut loops: Vec<Loop> = Vec::new();
        // Every loop still open: the index of a `for` or `select`, `None` for
        // a `while` or an `until`.
        let mut open: Vec<Option<usize>> = Vec::new();
        for (at, segment) in segments.iter().enumerate() {
            let mut rest = segment.words.as_slice();
            while let [word, after @ ..] = rest
                && word.is_bare()
            {
                rest = after;
                match word.text.as_str() {
                    "while" | "until" => open.push(None),
                    "done" => {
                        if let Some(Some(closed)) = open.pop() {
                            loops[closed].end = Some(at);
                        }
                    }
                    "for" | "select" => {
                        open.push(Some(loops.len()));
                        loops.push(header(word, after, segments, at));
                        break;
                    }
                    "if" | "then" | "else" | "elif" | "fi" | "do" | "esac" | "{" | "}" | "!" => {}
                    _ => break,
                }
            }
        }
        // A variable named anywhere but in its own loop's header and in plain
        // uses of it may be bound by something this does not follow.
        for read in &mut loops {
            let named_once = read
                .name
                .as_deref()
                .is_some_and(|name| words().map(|w| mentions(&w.shown, name)).sum::<usize>() == 1);
            if !named_once {
                read.values = None;
            }
        }
        Self {
            loops,
            splits: words().any(|w| w.shown.contains("IFS")),
        }
    }

    /// Every value `word`, in the segment at `at`, can have, when each of its
    /// expansions is a loop variable read here. `None` when it is anything
    /// else.
    pub(super) fn resolve(&self, word: &Word, at: usize) -> Option<Vec<String>> {
        let Expands::Variables(vars) = &word.expands else {
            return None;
        };
        // An option stays an expansion: what `-$d` turns out to be decides
        // what the program does with every word after it.
        if word.text.starts_with('-') || (!word.quoted && self.splits) {
            return None;
        }
        let mut spellings = vec![word.text.clone()];
        // Last first, so each splice leaves the earlier offsets where they
        // were.
        for var in vars.iter().rev() {
            let values = self.values(&var.name, at)?;
            spellings = spellings
                .iter()
                .flat_map(|spelling| {
                    values.iter().map(move |value| {
                        let mut spliced = spelling.clone();
                        spliced.replace_range(var.at..var.at + var.len, value);
                        spliced
                    })
                })
                .collect();
            if spellings.len() > MAX_SPELLINGS {
                return None;
            }
        }
        Some(spellings)
    }

    /// The values of `name` in the segment at `at`: those of the innermost
    /// loop over it whose body holds the segment.
    fn values(&self, name: &str, at: usize) -> Option<&[String]> {
        self.loops
            .iter()
            .filter(|l| l.name.as_deref() == Some(name))
            .filter(|l| l.start <= at && l.end.is_some_and(|end| at < end))
            .max_by_key(|l| l.start)?
            .values
            .as_deref()
    }
}

/// Read the header of a loop from the words after its `keyword`, in the
/// segment at `at`.
fn header(keyword: &Word, after: &[Word], segments: &[Segment], at: usize) -> Loop {
    let start = at + 1;
    let mut read = Loop {
        name: None,
        values: None,
        start,
        end: None,
    };
    let Some(name) = after
        .first()
        .filter(|name| name.is_bare() && is_variable_name(&name.text))
    else {
        return read;
    };
    read.name = Some(name.text.clone());
    let [_, word_in, list @ ..] = after else {
        return read;
    };
    // The body opens with `do` in the next segment; any other shape (zsh's
    // short loops, a brace body) is not one this follows.
    let opens_with_do = segments
        .get(start)
        .and_then(|s| s.words.first())
        .is_some_and(|w| w.is_bare() && w.text == "do");
    // `select` reads its value from a person.
    let spelled_out = keyword.text == "for"
        && word_in.is_bare()
        && word_in.text == "in"
        && opens_with_do
        && is_ordinary(&name.text)
        && !list.is_empty()
        && list.iter().all(is_spelled_out)
        && segments[at].reads.is_empty()
        && segments[at].writes.is_empty();
    if spelled_out {
        read.values = Some(list.iter().map(|w| w.text.clone()).collect());
    }
    read
}

/// Read the names a `for` or `select` header binds, which are `words` up to
/// its `in` or its `do`, and hand back what follows them. A name gets an
/// `env:NAME` key in `out` unless it is an ordinary one (see [`is_ordinary`]):
/// `for PATH in ..` changes what every later program resolves to, as
/// `PATH=..` does. `Err` with the word that is not a name the line spells
/// out.
pub(super) fn header_keys<'a>(
    words: &'a [Word],
    out: &mut Vec<String>,
) -> Result<&'a [Word], &'a Word> {
    let names = words
        .iter()
        .position(|w| w.is_bare() && matches!(w.text.as_str(), "in" | "do"))
        .unwrap_or(words.len());
    for name in &words[..names] {
        if !name.literal || !super::is_variable_name(&name.text) {
            return Err(name);
        }
        if !is_ordinary(&name.text) {
            out.push(super::env_key(&name.text));
        }
    }
    Ok(&words[names..])
}

/// Whether `name` is a variable no shell sets on its own: lower-case, and not
/// one of [`SHELL_SET`].
fn is_ordinary(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !SHELL_SET.contains(&name)
}

/// Whether a word of a loop's list is one value, known from the line: literal
/// text the shell neither expands nor splits, and not an option.
fn is_spelled_out(word: &Word) -> bool {
    word.literal
        && !word.text.is_empty()
        && !word.text.starts_with(['-', '='])
        && word
            .text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:,@%+=".contains(c))
}

/// How many times `shown` holds `name` other than as a plain use of the
/// variable (`$name`, `${name}`).
///
/// A flag is read differently from any other word. `-d` is a flag called `d`,
/// not the variable, while `-vd` may be `-v` with `d` glued on as the
/// variable it assigns.
fn mentions(shown: &str, name: &str) -> usize {
    if shown.starts_with('-') && !shown.starts_with("--") {
        return usize::from(shown.len() > name.len() + 1 && shown.ends_with(name));
    }
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    shown
        .match_indices(name)
        .filter(|(at, _)| {
            let (before, rest) = shown.split_at(*at);
            let after = rest.get(name.len()..).unwrap_or_default();
            let whole = !before.ends_with(is_name_char) && !after.starts_with(is_name_char);
            let plain_use =
                before.ends_with('$') || (before.ends_with("${") && after.starts_with('}'));
            whole && !plain_use
        })
        .count()
}

#[cfg(test)]
#[path = "loops_tests.rs"]
mod tests;
