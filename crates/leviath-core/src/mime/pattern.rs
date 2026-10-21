//! Mime type patterns: `image/png`, `image/*`, `*/*`.
//!
//! A pattern is what a person writes wherever a set of types is meant: a
//! region's `accepts`, a model's input list, a registry key. [`parse_pattern`]
//! checks one by the same rule the registry reads its keys with, so a program
//! that takes patterns from its own users accepts exactly what Leviath does,
//! and [`MimePattern::matches`] answers by the same code as
//! [`MimeType::matches`].

use std::fmt;

use super::{MimeType, MimeTypeError, is_token_char};

/// A checked mime pattern, lowercase: an exact type (`image/png`), a family
/// wildcard (`image/*`) or the catch-all (`*/*`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MimePattern(String);

/// Parse and normalise a pattern: trims and lowercases, and refuses anything
/// that is neither a mime type nor `type/*`.
///
/// `image/*` needs a `type` that could start a mime type; `*/*` is always
/// valid. Anything else must parse as a [`MimeType`], and fails the way that
/// would.
pub fn parse_pattern(raw: &str) -> Result<MimePattern, MimeTypeError> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.split_once('/') {
        Some((kind, "*")) if !kind.is_empty() => {
            if lower.contains(';') {
                Err(MimeTypeError::Parameter(raw.to_string()))
            } else if kind.chars().all(is_token_char) {
                Ok(MimePattern(lower))
            } else {
                Err(MimeTypeError::Character(raw.to_string()))
            }
        }
        _ => MimeType::parse(raw).map(|t| MimePattern(t.0)),
    }
}

impl MimePattern {
    /// Whether `mime_type` is one of the types this pattern names.
    pub fn matches(&self, mime_type: &MimeType) -> bool {
        matches_normalised(&self.0, mime_type)
    }

    /// The pattern as written, normalised.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<MimePattern> for String {
    fn from(pattern: MimePattern) -> Self {
        pattern.0
    }
}

impl fmt::Display for MimePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `mime_type` matches `pattern`, already trimmed and lowercased. A
/// pattern with no slash matches nothing.
pub(super) fn matches_normalised(pattern: &str, mime_type: &MimeType) -> bool {
    match pattern.split_once('/') {
        Some(("*", "*")) => true,
        Some((kind, "*")) => mime_type.kind() == kind,
        Some(_) => mime_type.0 == pattern,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> MimeType {
        MimeType::parse("image/png").unwrap()
    }

    #[test]
    fn exact_family_and_catch_all_patterns_parse_normalised() {
        assert_eq!(parse_pattern(" Image/PNG ").unwrap().as_str(), "image/png");
        assert_eq!(parse_pattern("IMAGE/*").unwrap().as_str(), "image/*");
        assert_eq!(parse_pattern("*/*").unwrap().to_string(), "*/*");
        assert_eq!(
            String::from(parse_pattern("model/*").unwrap()),
            "model/*".to_string()
        );
    }

    #[test]
    fn a_malformed_pattern_is_refused_with_why() {
        assert_eq!(
            parse_pattern("image"),
            Err(MimeTypeError::Shape("image".into()))
        );
        assert_eq!(parse_pattern("/*"), Err(MimeTypeError::Shape("/*".into())));
        assert_eq!(
            parse_pattern("image/*/x"),
            Err(MimeTypeError::Shape("image/*/x".into()))
        );
        assert_eq!(
            parse_pattern("im;age/*"),
            Err(MimeTypeError::Parameter("im;age/*".into()))
        );
        assert_eq!(
            parse_pattern("im age/*"),
            Err(MimeTypeError::Character("im age/*".into()))
        );
        assert_eq!(
            parse_pattern("image/png; q=1"),
            Err(MimeTypeError::Parameter("image/png; q=1".into()))
        );
    }

    #[test]
    fn a_pattern_matches_what_the_type_side_matches() {
        let png = png();
        for (raw, expected) in [
            ("image/png", true),
            ("image/*", true),
            ("*/*", true),
            ("image/jpeg", false),
            ("audio/*", false),
        ] {
            assert_eq!(parse_pattern(raw).unwrap().matches(&png), expected, "{raw}");
            assert_eq!(png.matches(raw), expected, "{raw}");
        }
        // The lenient side reads a slashless pattern as matching nothing.
        assert!(!png.matches("image"));
    }
}
