//! Typed content: mime types, parts, and where their bytes live.
//!
//! Leviath used to move text and nothing else. This module is the type system
//! that lets an image, an audio clip, a video, a document or a 3D model travel
//! through the same places text does: a context region, a tool result, a user
//! message, a model reply, a final output. Nothing here knows what an image
//! *is*. A [`MimeRegistry`] that users, blueprints and providers extend says
//! what each type is (its family, whether its bytes are text, how to estimate
//! its tokens), and everything else in the engine asks the registry rather
//! than matching on a type name.
//!
//! The one distinction the engine does draw is where bytes live. A part whose
//! registry row says `text = true` travels inline as a UTF-8 string. Any other
//! part is stored once per run under its SHA-256 and referenced by a
//! [`BlobRef`], so a journal, a snapshot or an event carries a few hundred
//! bytes of metadata rather than the file.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod cell;
pub mod check;
pub mod inbound;
pub mod inline_refs;
pub mod part;
pub mod probe;
pub mod registry;
pub mod store;

pub use cell::RegistryCell;
pub use check::{FnCheck, MimeCheck};
pub use inbound::InboundPart;
pub use part::{Blob, BlobRef, Delivery, Part, PartBody};
pub use registry::{MimeInfo, MimeRegistry, TokenRule};
pub use store::{BlobStore, MemoryBlobStore, is_sha256_hex, sha256_hex, verify_blob};

/// A mime type: `type/subtype`, lowercase, with no parameters.
///
/// Any string of that shape is valid. The registry decides what a type means;
/// this newtype only guarantees the shape, so a `match` on it is never the
/// right tool and a lookup is.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MimeType(String);

/// Why a string is not a mime type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MimeTypeError {
    /// No `/` separating type from subtype, or an empty half.
    #[error("'{0}' is not a mime type: expected type/subtype")]
    Shape(String),
    /// A `;charset=...` style parameter, which this newtype does not carry.
    #[error("'{0}' carries a parameter; mime types here are bare type/subtype")]
    Parameter(String),
    /// A character outside the token grammar.
    #[error("'{0}' has a character that cannot appear in a mime type")]
    Character(String),
}

impl MimeType {
    /// Parse and normalise: trims, lowercases, refuses parameters.
    pub fn parse(raw: &str) -> Result<Self, MimeTypeError> {
        let trimmed = raw.trim();
        if trimmed.contains(';') {
            return Err(MimeTypeError::Parameter(raw.to_string()));
        }
        let lower = trimmed.to_ascii_lowercase();
        let Some((kind, sub)) = lower.split_once('/') else {
            return Err(MimeTypeError::Shape(raw.to_string()));
        };
        if kind.is_empty() || sub.is_empty() || sub.contains('/') {
            return Err(MimeTypeError::Shape(raw.to_string()));
        }
        let ok = |c: char| c.is_ascii_alphanumeric() || "!#$&-^_.+*".contains(c);
        if !lower.chars().all(|c| c == '/' || ok(c)) {
            return Err(MimeTypeError::Character(raw.to_string()));
        }
        Ok(Self(lower))
    }

    /// The whole `type/subtype` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part before the slash.
    pub fn kind(&self) -> &str {
        self.0.split_once('/').map_or(self.0.as_str(), |(k, _)| k)
    }

    /// The part after the slash.
    pub fn subtype(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, s)| s)
    }

    /// Whether this type matches `pattern`, which may be exact (`image/png`),
    /// a family wildcard (`image/*`) or the catch-all (`*/*`).
    pub fn matches(&self, pattern: &str) -> bool {
        let pattern = pattern.trim().to_ascii_lowercase();
        match pattern.split_once('/') {
            Some(("*", "*")) => true,
            Some((kind, "*")) => self.kind() == kind,
            Some(_) => self.0 == pattern,
            None => false,
        }
    }

    /// Whether any pattern in `patterns` matches, as [`Self::matches`].
    pub fn matches_any<S: AsRef<str>>(&self, patterns: &[S]) -> bool {
        patterns.iter().any(|p| self.matches(p.as_ref()))
    }

    /// The `type/*` pattern that would match this type.
    pub fn family_pattern(&self) -> String {
        format!("{}/*", self.kind())
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MimeType({})", self.0)
    }
}

impl TryFrom<String> for MimeType {
    type Error = MimeTypeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl TryFrom<&str> for MimeType {
    type Error = MimeTypeError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<MimeType> for String {
    fn from(value: MimeType) -> Self {
        value.0
    }
}

impl std::str::FromStr for MimeType {
    type Err = MimeTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// The `text/plain` type, which every plain string is.
pub fn text_plain() -> MimeType {
    MimeType("text/plain".to_string())
}

/// The `application/octet-stream` type: bytes nothing could name.
pub fn octet_stream() -> MimeType {
    MimeType("application/octet-stream".to_string())
}

/// A byte count as people read it: `512 B`, `240 KB`, `18.2 MB`.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else {
        format!("{:.2} GB", b / KB / KB / KB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalises() {
        let t = MimeType::parse("  Image/PNG ").unwrap();
        assert_eq!(t.as_str(), "image/png");
        assert_eq!(t.kind(), "image");
        assert_eq!(t.subtype(), "png");
        assert_eq!(t.to_string(), "image/png");
        assert_eq!(format!("{t:?}"), "MimeType(image/png)");
        assert_eq!(t.family_pattern(), "image/*");
    }

    #[test]
    fn rejects_bad_shapes() {
        let shape = |s: &str| Err(MimeTypeError::Shape(s.to_string()));
        assert_eq!(MimeType::parse("png"), shape("png"));
        assert_eq!(MimeType::parse("/png"), shape("/png"));
        assert_eq!(MimeType::parse("image/"), shape("image/"));
        assert_eq!(MimeType::parse("a/b/c"), shape("a/b/c"));
        assert_eq!(
            MimeType::parse("text/plain; charset=utf-8"),
            Err(MimeTypeError::Parameter(
                "text/plain; charset=utf-8".to_string()
            ))
        );
        assert_eq!(
            MimeType::parse("image/p ng"),
            Err(MimeTypeError::Character("image/p ng".to_string()))
        );
        let err = MimeType::parse("png").unwrap_err();
        assert!(err.to_string().contains("type/subtype"));
        assert!(
            MimeType::parse("a;b")
                .unwrap_err()
                .to_string()
                .contains("parameter")
        );
        assert!(
            MimeType::parse("a/b c")
                .unwrap_err()
                .to_string()
                .contains("character")
        );
    }

    #[test]
    fn wildcard_matching() {
        let t = MimeType::parse("image/png").unwrap();
        assert!(t.matches("image/png"));
        assert!(t.matches("IMAGE/*"));
        assert!(t.matches("*/*"));
        assert!(!t.matches("audio/*"));
        assert!(!t.matches("image/jpeg"));
        assert!(!t.matches("image"));
        assert!(t.matches_any(&["audio/*", "image/*"]));
        assert!(!t.matches_any::<&str>(&[]));
    }

    #[test]
    fn serde_round_trip_and_conversions() {
        let t: MimeType = serde_json::from_str("\"Audio/WAV\"").unwrap();
        assert_eq!(t.as_str(), "audio/wav");
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"audio/wav\"");
        assert!(serde_json::from_str::<MimeType>("\"nope\"").is_err());
        let s: String = t.clone().into();
        assert_eq!(s, "audio/wav");
        let via_str: MimeType = "video/mp4".try_into().unwrap();
        assert_eq!(via_str.kind(), "video");
        let via_string: MimeType = String::from("model/obj").try_into().unwrap();
        assert_eq!(via_string.subtype(), "obj");
        let parsed: MimeType = "text/markdown".parse().unwrap();
        assert_eq!(parsed, MimeType::parse("text/markdown").unwrap());
        assert_eq!(text_plain().as_str(), "text/plain");
        assert_eq!(octet_stream().as_str(), "application/octet-stream");
        assert!(via_str < via_string || via_string < via_str);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(240 * 1024), "240 KB");
        assert_eq!(human_size(18 * 1024 * 1024 + 200 * 1024), "18.2 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }
}
