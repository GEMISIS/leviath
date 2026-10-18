//! Where a shell line's path operands point, so a safe-listed read stays a read
//! of the project.
//!
//! The safe list says `cat` and `grep` need no prompt, which is right for
//! `cat src/main.rs` and wrong for `cat ~/.ssh/id_rsa`. `read_file` already
//! refuses the second through `resolve_read`; this holds the shell to the same
//! fence by finding every word in a segment that could name a file and asking
//! the read fence about it.
//!
//! It is a heuristic over words, like the rest of this module, and it fails
//! towards asking: a word it cannot place (an expansion, a `cd` it cannot
//! follow) counts as outside, so a missed case costs a prompt rather than a
//! read nobody approved.

use std::path::{Path, PathBuf};

use super::{BACKSLASH_ESCAPES, KEY_PREFIX, Segment, Stripped, Word};

/// What in a line reaches outside the workdir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Outside {
    /// The keys (with [`KEY_PREFIX`]) of the segments that do. The safe list
    /// does not cover these; a grant still can.
    pub keys: Vec<String>,
    /// The operands that did it, as written, for the message a person or a
    /// model reads.
    pub operands: Vec<String>,
}

impl Outside {
    /// Whether nothing in the line leaves the workdir.
    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Programs that open no file named in their arguments, so none of their words
/// is a path operand. `echo /etc/hosts` prints a string.
///
/// Short on purpose. `date` is not here (`date -f FILE` reads one), nor `test`
/// (it reports whether a file exists), nor anything a user adds to
/// `[safe_commands]`: a program nobody has looked at has every word checked.
const NO_FILE_OPERANDS: &[&str] = &[
    "echo", "printf", "seq", "sleep", "true", "false", "pwd", "basename", "dirname", "uname",
    "hostname", "whoami", "id", "ps", "pgrep", "which", "type", "tr",
];

/// Programs whose first plain argument is a pattern or a program rather than a
/// file: `grep -rn "/api/" src` searches `src` for `/api/`, it does not read
/// `/api/`.
const PATTERN_FIRST: &[&str] = &["grep", "egrep", "fgrep", "jq"];

/// The flags that hand `grep` its pattern some other way, so its first plain
/// argument is a file after all.
const PATTERN_FLAGS: &[&str] = &["-e", "-f", "--regexp", "--file"];

/// Programs that move the shell, so a later segment's relative paths are
/// relative to somewhere else.
const MOVES_THE_SHELL: &[&str] = &["cd", "pushd"];

/// Which segments of `command` name a path outside `workdir` that `may_read`
/// refuses.
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
/// platforms' readings are testable on either.
pub(crate) fn outside_workdir_for(
    command: &str,
    backslash_escapes: bool,
    workdir: &Path,
    home: Option<&Path>,
    may_read: &dyn Fn(&Path) -> bool,
) -> Outside {
    // A line that will not tokenize has no keys, so nothing covers it anyway.
    let segments = super::tokenize_for(command, backslash_escapes).unwrap_or_default();
    let mut walk = Walk {
        bases: vec![workdir.to_path_buf()],
        home,
        may_read,
        left: false,
        out: Outside::default(),
    };
    for segment in &segments {
        walk.segment(segment);
    }
    walk.out.keys.sort();
    walk.out.keys.dedup();
    walk.out
}

/// The state of one line's walk.
struct Walk<'a> {
    /// Every directory a relative path in this segment could be relative to.
    /// The workdir, plus each `cd` target inside it seen so far: which of them
    /// the shell is really in depends on `&&`, `||` and subshells, so a path
    /// has to stay inside from all of them.
    bases: Vec<PathBuf>,
    home: Option<&'a Path>,
    may_read: &'a dyn Fn(&Path) -> bool,
    /// A `cd` left the workdir, so everything after it runs somewhere this
    /// does not follow.
    left: bool,
    out: Outside,
}

impl Walk<'_> {
    fn segment(&mut self, segment: &Segment) {
        // A segment that runs nothing opens nothing, and one this cannot read
        // already leaves the whole line without keys.
        let Stripped::Program { env_keys, words } = super::strip_to_program(&segment.words) else {
            return;
        };
        let keys = super::program_keys(env_keys, words);
        let mut offending: Vec<String> = Vec::new();
        if self.left {
            offending.push("(after a cd out of the working directory)".to_string());
        }
        let program = words[0].text.as_str();
        if MOVES_THE_SHELL.contains(&program) {
            self.move_shell(&words[1..], &mut offending);
        } else if !NO_FILE_OPERANDS.contains(&program) {
            for word in operands(program, &words[1..]).chain(&segment.reads) {
                self.check_word(word, &mut offending);
            }
        }
        if !offending.is_empty() {
            self.out
                .keys
                .extend(keys.into_iter().map(|k| format!("{KEY_PREFIX}{k}")));
            self.out.operands.extend(offending);
        }
    }

    /// A `cd`: its target is a path operand, and where it lands decides what
    /// every later segment's relative paths mean.
    fn move_shell(&mut self, args: &[Word], offending: &mut Vec<String>) {
        // No target is `$HOME`, and `-` is wherever the shell was before;
        // neither is a place this can vouch for.
        let Some(target) = args.iter().find(|w| !w.text.starts_with('-')) else {
            self.left = true;
            offending.push("cd".to_string());
            return;
        };
        let before = offending.len();
        self.check_word(target, offending);
        if offending.len() > before {
            self.left = true;
            return;
        }
        // Checked above, so the target expands and folds from every base.
        let target = self.expand(&target.text).unwrap_or_default();
        let moved: Vec<PathBuf> = self
            .bases
            .iter()
            .filter_map(|base| leviath_core::paths::fold_dot_dot(&base.join(&target)))
            .collect();
        self.bases.extend(moved);
    }

    /// `text` with a leading `~` expanded: `~` and `~/rest` are home, and
    /// `~name` (somebody else's home, which nothing here can look up) is
    /// `None`, as is `~` with no home to expand to.
    fn expand(&self, text: &str) -> Option<PathBuf> {
        match text.strip_prefix('~') {
            None => Some(PathBuf::from(text)),
            Some(rest) if rest.is_empty() || rest.starts_with('/') => self
                .home
                .map(|home| home.join(rest.trim_start_matches('/'))),
            Some(_) => None,
        }
    }

    /// Check one word, adding it to `offending` when it can reach outside.
    fn check_word(&self, word: &Word, offending: &mut Vec<String>) {
        if !word.literal {
            offending.push(format!("{} (expanded at run time)", word.text));
            return;
        }
        if candidates(&word.text).any(|text| !self.inside(text)) {
            offending.push(word.text.clone());
        }
    }

    /// Whether `text`, as a path, stays where `may_read` allows from every
    /// base the shell could be in.
    fn inside(&self, text: &str) -> bool {
        self.expand(text).is_some_and(|path| {
            self.bases
                .iter()
                .all(|base| (self.may_read)(&base.join(&path)))
        })
    }
}

/// The words of a segment that could name a file: everything after the
/// program, less the pattern a [`PATTERN_FIRST`] program takes first.
fn operands<'a>(program: &str, args: &'a [Word]) -> impl Iterator<Item = &'a Word> {
    let skip = match PATTERN_FIRST.contains(&program) && !names_its_pattern(args) {
        true => args.iter().position(|w| !w.text.starts_with('-')),
        false => None,
    };
    args.iter()
        .enumerate()
        .filter(move |(i, _)| Some(*i) != skip)
        .map(|(_, w)| w)
}

/// Whether a `grep` hands over its pattern with a flag, so no plain argument
/// is the pattern.
fn names_its_pattern(args: &[Word]) -> bool {
    args.iter()
        .any(|w| PATTERN_FLAGS.iter().any(|f| w.text.starts_with(f)))
}

/// The paths one word could name.
///
/// A plain word is one. A `--flag=value` is its value. A short flag with a
/// value glued on (`-f../list`, `-rf/etc/x`) could have it start after any
/// letter, so every tail that starts like a path is one; that can read a
/// harmless flag as an absolute path, which costs a prompt. A flag with
/// nothing path-like in it is none.
fn candidates(text: &str) -> Box<dyn Iterator<Item = &str> + '_> {
    if !text.starts_with('-') || text == "-" {
        return Box::new(std::iter::once(text));
    }
    if let Some((_, value)) = text.split_once('=') {
        return Box::new(std::iter::once(value));
    }
    if text.starts_with("--") {
        return Box::new(std::iter::empty());
    }
    // A value glued on starts right after a flag letter: the tail after the
    // first one, and any tail starting like a path straight after a letter.
    let letters: Vec<(usize, char)> = text.char_indices().collect();
    let tails = letters
        .windows(2)
        .skip(1)
        .filter(|pair| pair[0].1.is_ascii_alphabetic() && matches!(pair[1].1, '/' | '~' | '.'))
        .map(|pair| pair[1].0);
    let mut starts: Vec<usize> = std::iter::once(2).chain(tails).collect();
    starts.dedup();
    Box::new(
        starts
            .into_iter()
            .map(move |at| text.get(at..).unwrap_or_default())
            .filter(|tail| !tail.is_empty()),
    )
}

#[cfg(test)]
mod tests;
