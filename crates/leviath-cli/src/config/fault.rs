//! Why `config.toml` would not load, in enough detail to point at the spot.
//!
//! The loader produces a [`ConfigFault`]: the file, a one-line reason, and
//! where in the file it is, each reachable on its own. A flattened string is
//! enough for a command about to exit and print it, and not enough for
//! anything else, because the daemon keeps running on its last good config and
//! every surface that has to *explain* that - a JSON field, a dashboard banner
//! two lines tall, a doctor check - lays those pieces out differently.
//!
//! `Display` renders the same sentence the CLI prints, so a caller that only
//! wants the string can keep taking it.

use std::fmt;
use std::path::{Path, PathBuf};

/// Which step of loading refused the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultKind {
    /// The file exists and could not be read.
    Read,
    /// The bytes are not TOML this build can parse, or a value has the wrong
    /// type for the field it was given to.
    Parse,
    /// It parsed, and then one of its values was refused: an endpoint with no
    /// address, an MCP server with no command.
    Validation,
}

impl FaultKind {
    /// The word the API and the log use for this kind.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Parse => "parse",
            Self::Validation => "validation",
        }
    }
}

/// A config file that will not load, and everything a surface needs to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigFault {
    /// Which step refused it.
    pub(crate) kind: FaultKind,
    /// The file. Carried on the fault rather than assumed by each reader,
    /// because `LEVIATH_CONFIG_PATH` and `LEVIATH_HOME` both move it.
    pub(crate) path: PathBuf,
    /// One line, no caret art: what a banner, a JSON field or a status line
    /// shows.
    pub(crate) message: String,
    /// 1-based line, when the failure has a place in the file. A validation
    /// failure has none: it is about a value, not a byte offset.
    pub(crate) line: Option<usize>,
    /// 1-based column, alongside [`line`](Self::line).
    pub(crate) column: Option<usize>,
    /// The config key a validation failure is about, dotted
    /// (`model_providers.local`). `None` for a parse failure, which has a
    /// position instead.
    pub(crate) key: Option<String>,
    /// The full rendering, including the caret art `toml` draws under the
    /// offending line. What the log and `lev validate` print when there is
    /// room for more than one line.
    pub(crate) detail: String,
}

impl ConfigFault {
    /// The file is there and could not be read.
    pub(crate) fn read(path: &Path, e: &std::io::Error) -> Self {
        Self {
            kind: FaultKind::Read,
            path: path.to_path_buf(),
            message: e.to_string(),
            line: None,
            column: None,
            key: None,
            detail: format!("Failed to read config from '{}': {e}", path.display()),
        }
    }

    /// The bytes did not parse. `content` is what was parsed, and is what
    /// turns the error's byte span into a line and a column.
    pub(crate) fn parse(path: &Path, content: &str, e: &toml::de::Error) -> Self {
        // `unzip` rather than a match on the option: an error with no span is
        // rare enough that a branch for it would be a region no test reaches,
        // and "both or neither" is exactly what the two fields mean.
        let (line, column) = e
            .span()
            .map(|span| line_and_column(content, span.start))
            .unzip();
        Self {
            kind: FaultKind::Parse,
            path: path.to_path_buf(),
            message: one_line(e.message()),
            line,
            column,
            key: None,
            detail: format!("Failed to parse config: {e}"),
        }
    }

    /// It parsed and a value was refused. `key` is the dotted config key the
    /// refusal is about, which is the thing the user has to go and edit.
    pub(crate) fn validation(path: &Path, key: &str, message: &str) -> Self {
        Self {
            kind: FaultKind::Validation,
            path: path.to_path_buf(),
            message: one_line(message),
            line: None,
            column: None,
            key: Some(key.to_string()),
            detail: message.to_string(),
        }
    }

    /// Read `path` only to find out whether it loads, without keeping the
    /// config it produces.
    ///
    /// For the surfaces that report rather than serve: the dashboard header
    /// and `lev doctor` both want the answer and neither has any use for the
    /// `Config`. A missing file is not a fault - no config means defaults.
    pub(crate) fn check(path: &Path) -> Option<Self> {
        super::Config::read_file(path).err().map(|boxed| *boxed)
    }

    /// Where in the file, in words, when that is known.
    ///
    /// A parse failure has a position and a validation failure has a key, and
    /// a reader wants whichever there is without asking which kind it holds.
    pub(crate) fn location(&self) -> Option<String> {
        if let (Some(line), Some(column)) = (self.line, self.column) {
            return Some(format!("line {line}, column {column}"));
        }
        self.key.clone()
    }

    /// The whole fault on one line: where, then what.
    ///
    /// This is what fits in a banner, a spawn warning and a doctor check, so
    /// it is written once here rather than assembled slightly differently at
    /// each of them.
    pub(crate) fn summary(&self) -> String {
        match self.location() {
            Some(place) => format!("{place}: {}", self.message),
            None => self.message.clone(),
        }
    }
}

impl fmt::Display for ConfigFault {
    /// The sentence the CLI printed before any of this existed, so a caller
    /// that only ever wanted a string keeps getting the same one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for ConfigFault {}

/// Collapse a message onto one line, so a banner or a JSON field never gets a
/// newline it has no way to draw.
fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The 1-based line and column of a byte offset in `content`.
///
/// The column counts characters rather than bytes, matching how `toml` renders
/// its own position and how an editor counts: a comment in Japanese above the
/// broken line should not push the caret off the end of it.
fn line_and_column(content: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for (idx, ch) in content.char_indices() {
        // Stop at the character the offset lands in, rather than past it: a
        // span that names a byte in the middle of one should point at that
        // character, and an offset past the end simply consumes everything.
        if idx + ch.len_utf8() > offset {
            break;
        }
        match ch {
            '\n' => {
                line += 1;
                column = 1;
            }
            _ => column += 1,
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntax error keeps the place it happened, so a banner can point at it.
    #[test]
    fn a_syntax_error_carries_a_line_and_a_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_provider = \"anthropic\"\nbroken : :\n").unwrap();

        let fault = ConfigFault::check(&path).expect("a syntax error is a fault");
        assert_eq!(fault.kind, FaultKind::Parse);
        assert_eq!(fault.line, Some(2), "{fault:?}");
        assert_eq!(fault.column, Some(8), "{fault:?}");
        assert_eq!(fault.path, path);
        assert!(fault.key.is_none());
        // Bound first: a lazy format argument only evaluates on failure, and
        // the 100% gate counts it as a region nothing reached.
        let summary = fault.summary();
        assert!(summary.starts_with("line 2, column 8: "), "{summary}");
    }

    /// A refused value keeps the key it is about, which is what the user edits.
    #[test]
    fn a_validation_failure_carries_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[model_providers.local]\nkind = \"openai-compatible\"\n",
        )
        .unwrap();

        let fault = ConfigFault::check(&path).expect("an endpoint with no base_url is a fault");
        assert_eq!(fault.kind, FaultKind::Validation);
        assert_eq!(fault.key.as_deref(), Some("model_providers.local"));
        assert!(fault.line.is_none());
        let summary = fault.summary();
        assert!(summary.starts_with("model_providers.local: "), "{summary}");
    }

    /// A file that loads is no fault, and neither is one that is not there.
    #[test]
    fn a_good_file_and_a_missing_file_are_both_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(
            ConfigFault::check(&path).is_none(),
            "no file means defaults"
        );

        std::fs::write(&path, "default_provider = \"anthropic\"\n").unwrap();
        assert!(ConfigFault::check(&path).is_none());
    }

    /// A directory is not readable as a file, and says so without a position.
    #[test]
    fn an_unreadable_path_is_a_read_fault() {
        let dir = tempfile::tempdir().unwrap();
        let fault = ConfigFault::check(dir.path()).expect("a directory is not a config file");
        assert_eq!(fault.kind, FaultKind::Read);
        assert_eq!(fault.kind.as_str(), "read");
        assert!(fault.location().is_none());
        assert_eq!(fault.summary(), fault.message);
        assert!(fault.to_string().contains("Failed to read config from"));
    }

    /// The other two kinds name themselves for the API and the log.
    #[test]
    fn every_kind_has_a_word() {
        assert_eq!(FaultKind::Parse.as_str(), "parse");
        assert_eq!(FaultKind::Validation.as_str(), "validation");
    }

    /// A multi-byte character before the fault moves the column by one per
    /// character, not one per byte.
    #[test]
    fn the_column_counts_characters_rather_than_bytes() {
        assert_eq!(line_and_column("# ありがとう\nx", 0), (1, 1));
        // The `x` sits on line 2, whatever the comment above it cost in bytes.
        let content = "# ありがとう\nx";
        let offset = content.find('x').unwrap();
        assert_eq!(line_and_column(content, offset), (2, 1));
    }

    /// An offset that lands inside a character, or past the end, answers
    /// rather than panicking.
    #[test]
    fn an_offset_off_a_boundary_still_answers() {
        // Byte 3 is inside the second character of "あい".
        assert_eq!(line_and_column("あい", 4), (1, 2));
        assert_eq!(line_and_column("ab", 99), (1, 3));
    }

    /// A value of the wrong type is a parse failure too, and points at the
    /// value rather than at the key.
    #[test]
    fn a_wrongly_typed_value_points_at_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_provider = 7\n").unwrap();

        let fault = ConfigFault::check(&path).expect("a number is not a provider name");
        assert_eq!(fault.kind, FaultKind::Parse);
        assert_eq!(fault.line, Some(1), "{fault:?}");
        assert_eq!(fault.column, Some(20), "{fault:?}");
    }

    /// Whitespace and newlines are squeezed out of a message before it reaches
    /// a one-line surface.
    #[test]
    fn a_message_is_flattened_onto_one_line() {
        assert_eq!(one_line("two\nlines   here"), "two lines here");
    }
}
