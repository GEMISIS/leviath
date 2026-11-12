//! Options that decide what a line does: the builtins that change how the
//! shell reads the rest of it, and the ways a program's own option is spelled
//! where one turns a program that reads into one that writes or runs.
//!
//! Every reading this parser makes of a word (what is a glob, what a `~` or
//! an `=` at the start of a word turns into, where a word is split) is the
//! reading of a shell with its options as they start out. A line that turns
//! one of them on is read by a different shell from there on: under zsh's
//! `EXTENDED_GLOB` `^x` is every name but `x`, under `GLOB_DOTS` or bash's
//! `dotglob` a `*` reaches the dot files, under `SH_WORD_SPLIT` a quoted list
//! comes apart. So a line that changes the options is one no key can speak
//! for, apart from the handful of options that only say when to stop and
//! what to print.
//!
//! This sees the line, not the files the shell reads before it: a `setopt`
//! in a `~/.zshenv` is outside what the parser models, like a `$SHELL` that
//! is not POSIX at all.

use super::Word;

/// Builtins whose whole job is setting shell options or the emulation mode.
const SETS_OPTIONS: &[&str] = &["setopt", "unsetopt", "emulate", "shopt"];

/// The single-letter options of `set` that change nothing about how a word
/// is read: stop on an error (`e`), on an unset variable (`u`), trace (`x`),
/// echo input (`v`), do not glob (`f`), do not clobber (`C`), and `o`, which
/// names the option in the next word.
const HARMLESS_LETTERS: &str = "euxvfCo";

/// The options `set -o` may name, by the same rule.
const HARMLESS_NAMES: &[&str] = &[
    "errexit",
    "nounset",
    "pipefail",
    "xtrace",
    "verbose",
    "noglob",
    "noclobber",
];

/// The word of a segment that changes how the shell reads the words after
/// it, when the segment is one that does: `setopt extendedglob`, `shopt -s
/// dotglob`, `set -o shwordsplit`, `set -k`.
pub(super) fn changes_the_shell(words: &[Word]) -> Option<&Word> {
    let (first, args) = words.split_first()?;
    match first.text.as_str() {
        name if SETS_OPTIONS.contains(&name) => Some(first),
        "set" => risky_set_option(args),
        _ => None,
    }
}

/// The argument of a `set` that turns on something other than
/// [`HARMLESS_LETTERS`] and [`HARMLESS_NAMES`], or that only the shell can
/// read. What follows `--`, or the first word that is not an option, is the
/// positional parameters and changes no option.
fn risky_set_option(args: &[Word]) -> Option<&Word> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if !arg.literal {
            return Some(arg);
        }
        let letters = arg.text.strip_prefix(['-', '+']).filter(|l| *l != "-")?;
        if !letters.chars().all(|c| HARMLESS_LETTERS.contains(c)) {
            return Some(arg);
        }
        if letters.contains('o') {
            // `set -o` with nothing after it only lists the options.
            let name = args.next()?;
            if !name.literal || !HARMLESS_NAMES.contains(&name.text.as_str()) {
                return Some(name);
            }
        }
    }
    None
}

/// One option of a program, as a word of a line may spell it.
pub(super) enum Spelling {
    /// A letter, alone or in a cluster behind one dash: `-C`, `-bC`. A value
    /// glued behind another letter that holds this one reads as it too.
    Letter(char),
    /// A long name. `getopt_long` and git's own option parser take any
    /// prefix of a name that fits one option only, so `--comp` is
    /// `--compile` and `--e` is `--edit-description`; a value may follow an
    /// `=`. Every prefix counts here, whether or not the program would call
    /// it ambiguous.
    Long(&'static str),
}

impl Spelling {
    /// Whether `word` spells this option.
    pub(super) fn is(&self, word: &str) -> bool {
        match self {
            Self::Letter(letter) => {
                word.starts_with('-') && !word.starts_with("--") && word.contains(*letter)
            }
            Self::Long(name) => {
                let written = word.split('=').next().unwrap_or(word);
                word.starts_with(name) || (written.len() > 2 && name.starts_with(written))
            }
        }
    }
}

/// Options that turn an otherwise read-only program into one that writes a
/// file or runs another program, keyed by the program that accepts them.
///
/// This is a denylist, and a denylist has to be complete to be correct - so it
/// is used for exactly one thing: keeping an entry on the default safe list
/// that would otherwise have to be removed. `git`'s read-only subcommands are
/// most of what a coding agent does, but `--output=<file>` is a diff-machinery
/// option that `diff`, `log` and `show` all accept, and `git diff` is an exact
/// safe entry. Refusing the segment is cheaper than losing read-only git.
///
/// A program whose escape *cannot* be spelled as a flag does not belong here
/// and was removed from the safe list instead - see [`crate::approvals`], where
/// `uniq`'s output operand is the worked example. When in doubt, remove the
/// entry rather than extending this table: the entry is convenience, the rule
/// is the guarantee.
const ESCAPE_FLAGS: &[(&str, &[Spelling])] = &[
    // `git branch --edit-description` opens the configured editor.
    (
        "git",
        &[
            Spelling::Long("--output"),
            Spelling::Long("--edit-description"),
        ],
    ),
    // `jobs -x cmd` runs `cmd`.
    ("jobs", &[Spelling::Letter('x')]),
    // `file -C` compiles its magic file into a `.mgc` beside it.
    (
        "file",
        &[Spelling::Letter('C'), Spelling::Long("--compile")],
    ),
];

/// The word of this segment that hands a safe-listed program an option
/// letting it escape, when it carries one: `--output=x`, a separated
/// `--output x`, `--comp`, `-bC`. The program is found by its own name,
/// whatever directory the line names it from.
pub(super) fn escape_flag<'a>(program: &str, words: &'a [Word]) -> Option<&'a Word> {
    let program = super::operands::program_name(program);
    let (_, flags) = ESCAPE_FLAGS.iter().find(|(name, _)| *name == program)?;
    words
        .iter()
        .skip(1)
        .find(|w| flags.iter().any(|f| f.is(&w.text)))
}

/// The variable a `printf -v NAME` assigns, when the segment is one: its
/// name, or the word that stands where a name no key can carry would be.
/// bash, zsh and `sh` take `-v`, and bash takes it glued on (`-vNAME`). A
/// first argument only the shell can read may be that option.
pub(super) fn assigned(words: &[Word]) -> Option<Result<&str, &Word>> {
    // A first argument means there is a program in front of it.
    let first = words.get(1).filter(|_| words[0].text == "printf")?;
    if !first.literal {
        return Some(Err(first));
    }
    let glued = first.text.strip_prefix("-v")?;
    let (named, name) = match glued.is_empty() {
        true => words.get(2).map(|word| (word, word.text.as_str()))?,
        false => (first, glued),
    };
    Some(match named.literal && super::is_variable_name(name) {
        true => Ok(name),
        false => Err(named),
    })
}

#[cfg(test)]
#[path = "options_tests.rs"]
mod tests;
