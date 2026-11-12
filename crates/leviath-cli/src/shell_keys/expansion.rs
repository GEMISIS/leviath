//! Which `$` expansions run nothing and read no file.
//!
//! An expansion makes a word's value unknown ahead of time, and most of this
//! module's callers treat every one alike: the word is not literal. One
//! caller does not. A program that opens no file named in its arguments
//! (`echo`, see `operands`) can be handed a variable's value without the line
//! reading anything a person did not see, as long as the expansion itself
//! does nothing but produce that value. This is the list of forms that do
//! nothing else, written as a grammar so that anything outside it is not let
//! through.
//!
//! The line may be run by `zsh`, `bash` or `sh` (see
//! `leviath_tools::BuiltinTools::detect_shell_for`), so a form is plain only
//! when it is harmless in all three:
//!
//! ```text
//! $NAME  $1  $@ $* $# $? $$ $! $-        a parameter, by name or by sign
//! $NAME[SUB]                             zsh's subscript without braces
//! ${NAME}  ${1}  ${@} ${*} ${?} ${$}  ${#}
//! ${NAME[SUB]}                           SUB is digits, -digits, @ or *
//! ${#NAME}  ${#NAME[SUB]}                a length
//! ${P:-WORD} ${P-WORD} ${P:+WORD} ${P+WORD}
//! ${P#PAT} ${P##PAT} ${P%PAT} ${P%%PAT}
//! ${P:N}  ${P:N:N}                       N is digits
//! ```
//!
//! `WORD` is harmless text and plain expansions; `PAT` is harmless text and
//! the wildcards `*` and `?`, which there match the value and no file.
//! Harmless text is letters, digits, spaces and `_-./,:@+=%`: nothing that
//! quotes, substitutes, redirects, globs or names a home directory.
//!
//! What is left out, and why:
//!
//! - `$(...)`, backticks and `$(<file)` run a command or read a file.
//! - `$((...))` and a subscript or an offset that names a variable are
//!   arithmetic, and `bash` evaluates a variable's value there as an
//!   expression, command substitutions included. Arithmetic over digits and
//!   operators alone is let through.
//! - `${!NAME}` reads the variable another one names, through the same
//!   arithmetic when that name carries a subscript.
//! - `${NAME=WORD}`, `${NAME:=WORD}` and zsh's `${NAME::=WORD}` assign.
//! - `${NAME@P}`, `${(e)NAME}`, `${(P)NAME}`, `${~NAME}` and the rest of
//!   zsh's flags evaluate, glob or redirect the value; `$NAME:h` and its
//!   kin resolve it against the file system.
//! - `$'...'` and `$"..."` are quoting, read by each shell its own way.

use std::iter::Peekable;
use std::str::Chars;

/// A variable a word expands by name alone, and where its text sits in the
/// word's [`text`](super::Word): the `d` of `$d`, the `{d}` of `${d}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VarRef {
    /// The variable.
    pub(super) name: String,
    /// Where the expansion's text starts in the word's text, in bytes.
    pub(super) at: usize,
    /// How many bytes of the word's text it takes.
    pub(super) len: usize,
}

/// What a word's expansions are, taken together.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) enum Expands {
    /// It has none: the word is literal.
    #[default]
    Nothing,
    /// Variables by name alone, so the word's value is its text with each
    /// one's value put in.
    Variables(Vec<VarRef>),
    /// Plain expansions that run nothing and read no file, at least one of
    /// them more than a variable by name.
    Plain,
    /// Something else: a substitution, arithmetic over a variable, an
    /// assignment, a form this does not read.
    Unknown,
}

impl Expands {
    /// The word gained a plain expansion: `var` when it is a variable by name
    /// alone.
    pub(super) fn plain(&mut self, var: Option<VarRef>) {
        *self = match (std::mem::take(self), var) {
            (Self::Unknown, _) => Self::Unknown,
            (Self::Nothing, Some(var)) => Self::Variables(vec![var]),
            (Self::Variables(mut vars), Some(var)) => {
                vars.push(var);
                Self::Variables(vars)
            }
            (_, None) | (Self::Plain, _) => Self::Plain,
        };
    }

    /// Whether every expansion in the word runs nothing and reads no file.
    /// True of a literal word, which has none.
    pub(super) fn is_plain(&self) -> bool {
        *self != Self::Unknown
    }
}

/// One plain expansion: what follows the `$`, and the variable when it is one
/// by name alone.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plain {
    /// The text after the `$`.
    pub(super) text: String,
    /// The variable, for `$NAME` and `${NAME}`.
    pub(super) var: Option<String>,
}

/// Parameters named by a sign rather than a word.
const SIGNS: &str = "@*#?$!-";

/// The signs a braced expansion may name. `${!...}` is indirection and
/// `${-...}` is read as an operator, so neither is here.
const BRACED_SIGNS: &str = "@*?$";

/// Read the plain expansion that follows a `$`, leaving `chars` just past it.
/// `None` when what follows is not one, with `chars` wherever the reading
/// stopped: the caller hands in a copy.
pub(super) fn take_plain(chars: &mut Peekable<Chars>) -> Option<Plain> {
    let first = *chars.peek()?;
    let mut text = String::new();
    if first == '{' {
        chars.next();
        text.push('{');
        let var = braced(chars, &mut text)?;
        return Some(Plain { text, var });
    }
    if is_name_start(first) {
        let name = take_name(chars);
        text.push_str(&name);
        return match chars.peek().copied() {
            // zsh reads a subscript here; the others read a literal `[`.
            Some('[') => {
                subscript(chars, &mut text)?;
                Some(Plain { text, var: None })
            }
            // zsh reads `$NAME:h` as a modifier applied to the value.
            Some(':') if modifier_follows(chars.clone()) => None,
            _ => Some(Plain {
                text,
                var: Some(name),
            }),
        };
    }
    if first.is_ascii_digit() || SIGNS.contains(first) {
        chars.next();
        text.push(first);
        return Some(Plain { text, var: None });
    }
    None
}

/// Skip a `$'...'` from its opening quote through its closing one. bash and
/// zsh read `\'` inside it as a quote that does not close it; a POSIX `sh`
/// reads a `$` and then a plain quoted word, which that quote does close.
/// `None` where the two end it at different places, or it never ends.
pub(super) fn take_ansi_c(chars: &mut Peekable<Chars>) -> Option<()> {
    chars.next();
    loop {
        match chars.next()? {
            '\'' => return Some(()),
            // The guard takes the escaped character either way.
            '\\' if chars.next()? == '\'' => return None,
            _ => {}
        }
    }
}

/// Take the text of a `$(...)` up to the `)` that closes it, leaving `chars`
/// just past it. A parenthesis between quotes, behind a backslash or in a
/// comment closes nothing, as in the shell. `None` when it never closes.
pub(super) fn take_substitution(chars: &mut Peekable<Chars>, escapes: bool) -> Option<String> {
    let mut depth = 0usize;
    let mut inner = String::new();
    // Whether a `#` here would start a word, and so a comment.
    let mut between_words = true;
    loop {
        let c = chars.next()?;
        if c == ')' && depth == 0 {
            return Some(inner);
        }
        inner.push(c);
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '\'' => loop {
                let quoted = chars.next()?;
                inner.push(quoted);
                if quoted == '\'' {
                    break;
                }
            },
            '"' => double_quoted(chars, escapes, &mut inner)?,
            '\\' if escapes => inner.push(chars.next()?),
            '#' if escapes && between_words => {
                while let Some(noted) = chars.next_if(|c| *c != '\n') {
                    inner.push(noted);
                }
            }
            _ => {}
        }
        between_words = ends_a_word(c);
    }
}

/// Copy a double-quoted string from just past its opening quote through its
/// closing one. A `$(...)` inside it is read as one, since a quote in there
/// is not this string's.
fn double_quoted(chars: &mut Peekable<Chars>, escapes: bool, text: &mut String) -> Option<()> {
    loop {
        let c = chars.next()?;
        text.push(c);
        match c {
            '"' => return Some(()),
            '\\' if escapes => text.push(chars.next()?),
            '$' if chars.next_if_eq(&'(').is_some() => {
                text.push('(');
                text.push_str(&take_substitution(chars, escapes)?);
                text.push(')');
            }
            _ => {}
        }
    }
}

/// Whether the `${...}` whose `{` `chars` is at closes with no quote between
/// its braces. The shells do not agree on what a quote opens in there, so one
/// that holds a quote is not something this reader can find the end of.
pub(super) fn braces_hold_no_quote(chars: Peekable<Chars>) -> bool {
    let mut depth = 0usize;
    for c in chars {
        match c {
            '{' => depth += 1,
            '}' if depth == 1 => return true,
            '}' => depth -= 1,
            '\'' | '"' | '`' => return false,
            _ => {}
        }
    }
    false
}

/// Whether `text` holds an expansion that can assign a variable: a `${...}`
/// with an `=` in it (`${x:=y}`, zsh's `${x::=y}`), or arithmetic with an
/// `=`, a `++` or a `--`. Read off the text alone, so it also says yes to an
/// `==` in arithmetic.
pub(super) fn may_assign(text: &str) -> bool {
    let holds = |open: &str, close: &str, marks: &[&str]| {
        text.split(open).skip(1).any(|after| {
            let inside = after.split(close).next().unwrap_or(after);
            marks.iter().any(|mark| inside.contains(mark))
        })
    };
    holds("${", "}", &["="])
        || holds("$((", "))", &["=", "++", "--"])
        || holds("$[", "]", &["=", "++", "--"])
}

/// Whether a word cannot run on past `c`, so that a `#` straight after it
/// starts a comment: a blank, a newline or an operator, as the shell counts
/// them. A carriage return, or any other space, is a character of a word.
pub(super) fn ends_a_word(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
    )
}

/// Whether a letter follows the `:` `chars` is at.
fn modifier_follows(mut chars: Peekable<Chars>) -> bool {
    chars.next();
    chars.peek().is_some_and(char::is_ascii_alphabetic)
}

/// Read a `${...}` from just past its `{` through its `}`, appending what is
/// read to `text`. The variable when the whole of it is `${NAME}`; `None`
/// inside when it is anything more.
fn braced(chars: &mut Peekable<Chars>, text: &mut String) -> Option<Option<String>> {
    let counted = chars.peek() == Some(&'#');
    if counted {
        chars.next();
        text.push('#');
        // `${#}` is the number of arguments.
        if chars.peek() == Some(&'}') {
            chars.next();
            text.push('}');
            return Some(None);
        }
    }
    let first = *chars.peek()?;
    let mut var = None;
    if is_name_start(first) {
        let name = take_name(chars);
        text.push_str(&name);
        match chars.peek() {
            Some('[') => subscript(chars, text)?,
            _ => var = Some(name),
        }
    } else if first.is_ascii_digit() {
        take_digits(chars, text);
    } else if BRACED_SIGNS.contains(first) {
        chars.next();
        text.push(first);
    } else {
        return None;
    }
    let operator = chars.next()?;
    text.push(operator);
    match operator {
        '}' => Some(var.filter(|_| !counted)),
        // A length takes no operator.
        _ if counted => None,
        '-' | '+' => word(chars, text).map(|()| None),
        '#' | '%' => {
            if chars.peek() == Some(&operator) {
                chars.next();
                text.push(operator);
            }
            pattern(chars, text).map(|()| None)
        }
        ':' => after_colon(chars, text).map(|()| None),
        _ => None,
    }
}

/// What follows the `:` of a `${P:...}`: a default (`:-`, `:+`) or a
/// substring by digits (`:N`, `:N:N`).
fn after_colon(chars: &mut Peekable<Chars>, text: &mut String) -> Option<()> {
    match *chars.peek()? {
        sign @ ('-' | '+') => {
            chars.next();
            text.push(sign);
            word(chars, text)
        }
        digit if digit.is_ascii_digit() => {
            take_digits(chars, text);
            if chars.peek() == Some(&':') {
                chars.next();
                text.push(':');
                chars.peek().filter(|c| c.is_ascii_digit())?;
                take_digits(chars, text);
            }
            closing(chars, text)
        }
        _ => None,
    }
}

/// The `}` that ends an expansion.
fn closing(chars: &mut Peekable<Chars>, text: &mut String) -> Option<()> {
    (chars.next()? == '}').then(|| text.push('}'))
}

/// A default's word through its `}`: harmless text and plain expansions.
fn word(chars: &mut Peekable<Chars>, text: &mut String) -> Option<()> {
    loop {
        match chars.next()? {
            '}' => {
                text.push('}');
                return Some(());
            }
            '$' => {
                text.push('$');
                text.push_str(&take_plain(chars)?.text);
            }
            c if is_harmless(c) => text.push(c),
            _ => return None,
        }
    }
}

/// A pattern through its `}`: harmless text and the wildcards that match
/// against the value.
fn pattern(chars: &mut Peekable<Chars>, text: &mut String) -> Option<()> {
    loop {
        match chars.next()? {
            '}' => {
                text.push('}');
                return Some(());
            }
            c if is_harmless(c) || matches!(c, '*' | '?') => text.push(c),
            _ => return None,
        }
    }
}

/// A subscript from its `[` through its `]`: digits, `-digits`, `@` or `*`.
/// A name here would be arithmetic over that variable's value.
fn subscript(chars: &mut Peekable<Chars>, text: &mut String) -> Option<()> {
    chars.next();
    text.push('[');
    match *chars.peek()? {
        all @ ('@' | '*') => {
            chars.next();
            text.push(all);
        }
        _ => {
            if chars.peek() == Some(&'-') {
                chars.next();
                text.push('-');
            }
            chars.peek().filter(|c| c.is_ascii_digit())?;
            take_digits(chars, text);
        }
    }
    (chars.next()? == ']').then(|| text.push(']'))
}

/// Take a run of digits.
fn take_digits(chars: &mut Peekable<Chars>, text: &mut String) {
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
        text.push(digit);
    }
}

/// Take a variable name. Called at a character that starts one.
fn take_name(chars: &mut Peekable<Chars>) -> String {
    let mut name = String::new();
    while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
        name.push(c);
    }
    name
}

/// Whether `c` starts a variable name.
fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Whether `c` is text that does nothing inside a default or a pattern: it
/// does not quote, substitute, redirect, separate commands, glob or name a
/// home directory.
fn is_harmless(c: char) -> bool {
    c.is_ascii_alphanumeric() || " _-./,:@+=%".contains(c)
}

/// Whether the text of a `$((...))` computes over digits and operators alone.
/// A name in it is a variable, whose value `bash` evaluates as an expression.
pub(super) fn arithmetic_is_inert(inner: &str) -> bool {
    inner
        .chars()
        .all(|c| c.is_ascii_digit() || " \t+-*/%()<>=!&|^~?:,".contains(c))
}

/// Whether what follows a `<` is the rest of zsh's numeric glob: `<1-5>`,
/// `<->`, `<10->`. zsh expands it against the names in a directory; `bash`
/// and `sh` read a redirect from `1-5` and another to whatever comes next.
pub(super) fn is_numeric_glob(mut chars: Peekable<Chars>) -> bool {
    let mut dash = false;
    loop {
        match chars.next() {
            Some(digit) if digit.is_ascii_digit() => {}
            Some('-') if !dash => dash = true,
            Some('>') => return dash,
            _ => return false,
        }
    }
}

#[cfg(test)]
#[path = "expansion_tests.rs"]
mod tests;
