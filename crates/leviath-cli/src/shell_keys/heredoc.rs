//! Heredocs: the delimiter a `<<` names, and the body that follows the line.
//!
//! A heredoc's body is stdin data, not part of the command line, so the
//! tokenizer reads the delimiter where the operator is and consumes the body
//! when the line's newline arrives. What cannot be trusted as data keeps the
//! line unreadable: see [`take_heredoc_body`].

/// A heredoc a line has opened, waiting for the newline its body starts after.
pub(super) struct Heredoc {
    /// The word that ends the body, matched against whole lines.
    pub(super) delimiter: String,
    /// Whether the delimiter carried any quoting. A quoted delimiter makes the
    /// body pure data; an unquoted one leaves it subject to expansion, so a
    /// command substitution inside it would really run.
    pub(super) literal: bool,
    /// `<<-`: the shell strips leading tabs from body and delimiter lines.
    pub(super) strip_tabs: bool,
}

/// Read the delimiter word after `<<`, with its quoting.
///
/// Quote concatenation applies the way it does to any word (`<<'EO'F` ends at
/// `EOF`), and any quoted piece - or a backslash escape - marks the whole
/// delimiter quoted. `None` for a quote that never closes, or for no word at
/// all (`<< ;`, or the third `<` of a here-string), where the shell would be
/// reading something this cannot.
pub(super) fn take_heredoc_delimiter(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    escapes: bool,
    strip_tabs: bool,
) -> Option<Heredoc> {
    let mut delimiter = String::new();
    let mut literal = false;
    loop {
        match chars.peek() {
            Some('\'') => {
                chars.next();
                literal = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        q => delimiter.push(q),
                    }
                }
            }
            Some('"') => {
                chars.next();
                literal = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        q => delimiter.push(q),
                    }
                }
            }
            Some('\\') if escapes => {
                chars.next();
                literal = true;
                delimiter.push(chars.next()?);
            }
            Some(&c)
                if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>') =>
            {
                break;
            }
            Some(&c) => {
                chars.next();
                delimiter.push(c);
            }
            None => break,
        }
    }
    match delimiter.is_empty() {
        true => None,
        false => Some(Heredoc {
            delimiter,
            literal,
            strip_tabs,
        }),
    }
}

/// Consume one heredoc's body, leaving `chars` just past the delimiter line.
///
/// `None` in the two cases where the line has to stay unreadable: the body
/// never ends (the shell would keep reading input that is not here), and an
/// unquoted body holding a `` ` `` or `$(`, or a line that ends in a
/// backslash and so could be joined into one (the shell expands such a body,
/// so a substitution inside it really runs - and could write - which is
/// exactly what this reader exists to not guess about). A quoted body is data
/// whatever it contains.
pub(super) fn take_heredoc_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    heredoc: &Heredoc,
) -> Option<()> {
    loop {
        let mut line = String::new();
        let mut line_ended = false;
        for c in chars.by_ref() {
            if c == '\n' {
                line_ended = true;
                break;
            }
            line.push(c);
        }
        let candidate = match heredoc.strip_tabs {
            true => line.trim_start_matches('\t'),
            false => line.as_str(),
        };
        if candidate == heredoc.delimiter {
            return Some(());
        }
        // A backslash that ends a line of an unquoted body joins it to the
        // next, which can put a `$` in front of a `(`; and an expansion in
        // one can assign a variable.
        let runs = line.contains('`')
            || line.contains("$(")
            || line.ends_with('\\')
            || super::expansion::may_assign(&line);
        if !heredoc.literal && runs {
            return None;
        }
        if !line_ended {
            return None;
        }
    }
}
