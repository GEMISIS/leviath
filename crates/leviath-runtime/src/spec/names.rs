//! Checked names for the things a run refers to.
//!
//! Every name is checked once, when it is made, and after that it cannot be
//! wrong. A stage name is never confused with a region name, a tool name never
//! carries a stray space, and a run id is always safe to use as a directory.
//! Each one serializes as its plain text.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a piece of text is not a valid name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The text was empty.
    #[error("a {what} cannot be empty")]
    Empty {
        /// Which kind of name was being made.
        what: &'static str,
    },
    /// The text was longer than the kind allows.
    #[error("a {what} is at most {max} bytes, and {got:?} is {len}")]
    TooLong {
        /// Which kind of name was being made.
        what: &'static str,
        /// The longest allowed.
        max: usize,
        /// The length given.
        len: usize,
        /// The text given.
        got: String,
    },
    /// The text held a character the kind does not allow.
    #[error("a {what} {rule}, and {got:?} does not")]
    Shape {
        /// Which kind of name was being made.
        what: &'static str,
        /// What the kind requires, as a clause ("uses only ...").
        rule: &'static str,
        /// The text given.
        got: String,
    },
}

fn check_len(what: &'static str, s: &str, max: usize) -> Result<(), NameError> {
    if s.is_empty() {
        return Err(NameError::Empty { what });
    }
    if s.len() > max {
        return Err(NameError::TooLong {
            what,
            max,
            len: s.len(),
            got: s.to_string(),
        });
    }
    Ok(())
}

fn shape(what: &'static str, rule: &'static str, s: &str, ok: bool) -> Result<(), NameError> {
    match ok {
        true => Ok(()),
        false => Err(NameError::Shape {
            what,
            rule,
            got: s.to_string(),
        }),
    }
}

/// A label people pick: no control characters and no space at either end.
fn label(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 128)?;
    let ok = !s.chars().any(char::is_control) && s.trim() == s;
    shape(
        what,
        "has no control characters and no space at either end",
        s,
        ok,
    )
}

/// An absolute path to a directory: no control characters, and at most as
/// long as a path a filesystem takes.
fn absolute_path(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 4096)?;
    let ok = !s.chars().any(char::is_control) && std::path::Path::new(s).is_absolute();
    shape(
        what,
        "is an absolute path with no control characters",
        s,
        ok,
    )
}

/// An identifier a flag or a template can name: a letter or `_`, then letters,
/// digits, `_` or `-`.
fn ident(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 64)?;
    let mut chars = s.chars();
    let first = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    shape(
        what,
        "starts with a letter or `_` and uses only letters, digits, `_` and `-`",
        s,
        first && rest,
    )
}

/// A tool name as a provider accepts it: letters, digits, `_`, `-` and `.`.
fn tool(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 128)?;
    let ok = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    shape(what, "uses only letters, digits, `_`, `-` and `.`", s, ok)
}

/// A provider's own id for a model or a route: anything without whitespace or
/// control characters.
fn opaque(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 256)?;
    let ok = !s.chars().any(|c| c.is_whitespace() || c.is_control());
    shape(what, "has no whitespace or control characters", s, ok)
}

/// A run id: one safe path component, since it names the run's directory.
fn run_id(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 128)?;
    shape(
        what,
        "uses only letters, digits, `.`, `_` and `-`, and is not `.` or `..`",
        s,
        leviath_core::is_safe_path_component(s),
    )
}

/// 64 lowercase hex digits: a sha256.
fn sha256_hex(what: &'static str, s: &str) -> Result<(), NameError> {
    let ok = s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    shape(what, "is 64 lowercase hex digits", s, ok)
}

/// A mime type or pattern: `type/subtype`, either half may be `*`.
fn mime_pattern(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 128)?;
    let token = |t: &str| {
        t == "*"
            || (!t.is_empty()
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c)))
    };
    let ok = s.split_once('/').is_some_and(|(a, b)| token(a) && token(b));
    shape(what, "is `type/subtype`, either half may be `*`", s, ok)
}

/// A path inside the run's workdir: relative, `/`-separated, and never
/// climbing out with `..`.
fn workdir_path(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 1024)?;
    let drive_letter = s.as_bytes().get(1) == Some(&b':');
    let ok = !s.starts_with('/')
        && !s.contains('\\')
        && !drive_letter
        && !s.chars().any(char::is_control)
        && s.split('/').all(|part| !part.is_empty() && part != "..");
    shape(
        what,
        "is relative to the workdir, uses `/`, and has no empty or `..` parts",
        s,
        ok,
    )
}

/// An `http` or `https` URL with a host.
fn http_url(what: &'static str, s: &str) -> Result<(), NameError> {
    check_len(what, s, 2048)?;
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"));
    let ok = rest.is_some_and(|r| {
        let host = r.split(['/', '?', '#']).next().unwrap_or_default();
        !host.is_empty() && !r.chars().any(|c| c.is_whitespace() || c.is_control())
    });
    shape(what, "is an http:// or https:// URL with a host", s, ok)
}

/// An MCP server name, by the same rule the MCP config uses.
fn mcp_server(what: &'static str, s: &str) -> Result<(), NameError> {
    shape(
        what,
        "uses only letters, digits, `_` and `-`",
        s,
        leviath_core::mcp_names::validate_server_name(s).is_ok(),
    )
}

macro_rules! name_type {
    ($(#[$doc:meta])* $name:ident, $what:literal, $check:path) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "description": concat!("A ", $what, ".") })
            }
        }

        impl $name {
            #[doc = concat!("Check `text` as a ", $what, ".")]
            pub fn new(text: impl Into<String>) -> Result<Self, NameError> {
                let text = text.into();
                $check($what, &text)?;
                Ok(Self(text))
            }

            #[doc = concat!("The ", $what, " as text.")]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = NameError;
            fn try_from(text: String) -> Result<Self, NameError> {
                Self::new(text)
            }
        }

        impl FromStr for $name {
            type Err = NameError;
            fn from_str(text: &str) -> Result<Self, NameError> {
                Self::new(text)
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> String {
                name.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }
    };
}

name_type!(
    /// A run's id, and the name of its directory.
    RunId, "run id", run_id
);
name_type!(
    /// An installed blueprint's name.
    BlueprintName, "blueprint name", label
);
name_type!(
    /// The absolute directory a blueprint that is not installed is read
    /// from: the one holding its `agent.toml`.
    ///
    /// Only a caller on this machine names a blueprint this way (the CLI, the
    /// control socket, an embedding program). A request that arrives over the
    /// network is refused one, so a remote caller cannot have the daemon read
    /// whatever directory it likes; see
    /// [`SpawnRequest::check_remote`](crate::spec::request::SpawnRequest::check_remote).
    BlueprintPath, "blueprint path", absolute_path
);

impl BlueprintPath {
    /// The directory, as a path.
    pub fn path(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }
}
name_type!(
    /// A stage in a run graph.
    StageName, "stage name", label
);
name_type!(
    /// A context region in a run graph.
    RegionName, "region name", label
);
name_type!(
    /// An edge out of a stage. Unique among the edges leaving that stage.
    EdgeName, "edge name", label
);
name_type!(
    /// A tool a model can call.
    ToolName, "tool name", tool
);
name_type!(
    /// A declared input of a run graph.
    InputName, "input name", ident
);
name_type!(
    /// One option of a `choice` input.
    ChoiceName, "choice option", ident
);
name_type!(
    /// A configured provider.
    ProviderName, "provider name", opaque
);
name_type!(
    /// A provider's id for a model.
    ModelId, "model id", opaque
);
name_type!(
    /// A named yolo profile.
    ProfileName, "profile name", label
);
name_type!(
    /// A sha256 digest: how code, blobs and blueprint revisions are addressed.
    Digest, "digest", sha256_hex
);
name_type!(
    /// A mime type or pattern a file input or region accepts: `image/*`.
    MimePattern, "mime pattern", mime_pattern
);
name_type!(
    /// A path inside the run's workdir.
    WorkdirPath, "workdir path", workdir_path
);
name_type!(
    /// An http or https URL.
    HttpUrl, "URL", http_url
);
name_type!(
    /// A configured MCP server.
    McpServerName, "MCP server name", mcp_server
);

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest as _;
        Self(hex::encode(sha2::Sha256::digest(bytes)))
    }
}

/// A model to run, and optionally the provider to run it on.
///
/// With no provider, the operator's `provider_order` picks one at spawn.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    /// The provider to use, when the caller names one.
    #[serde(default)]
    pub provider: Option<ProviderName>,
    /// The model.
    pub model: ModelId,
}

impl ModelRef {
    /// Parse `provider/model` or a bare `model`. The provider is the text
    /// before the first `/`; everything after it is the model id, which may
    /// hold more slashes (a gateway route such as `openai/gpt-5`).
    pub fn parse(text: &str) -> Result<Self, NameError> {
        match text.split_once('/') {
            Some((provider, model)) => Ok(Self {
                provider: Some(ProviderName::new(provider)?),
                model: ModelId::new(model)?,
            }),
            None => Ok(Self {
                provider: None,
                model: ModelId::new(text)?,
            }),
        }
    }

    /// The provider's name, or empty text when the reference names none.
    pub fn provider_or_empty(&self) -> &str {
        self.provider.as_ref().map_or("", ProviderName::as_str)
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.provider {
            Some(p) => write!(f, "{p}/{}", self.model),
            None => write!(f, "{}", self.model),
        }
    }
}

/// An installed blueprint, optionally pinned to one revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlueprintRef {
    /// The blueprint's name.
    pub name: BlueprintName,
    /// The revision to run. `None` runs whatever is installed.
    #[serde(default)]
    pub digest: Option<Digest>,
}

impl BlueprintRef {
    /// Parse `name` or `name@digest`.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        match text.rsplit_once('@') {
            Some((name, digest)) => Ok(Self {
                name: BlueprintName::new(name)?,
                digest: Some(Digest::new(digest)?),
            }),
            None => Ok(Self {
                name: BlueprintName::new(text)?,
                digest: None,
            }),
        }
    }
}

impl fmt::Display for BlueprintRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.digest {
            Some(d) => write!(f, "{}@{d}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blueprint_path_is_an_absolute_directory() {
        let here = std::env::temp_dir().join("agents").join("coder");
        let path = BlueprintPath::new(here.to_string_lossy()).unwrap();
        assert_eq!(path.path(), here.as_path());
        let deep = std::env::temp_dir().join("deep/".repeat(100));
        assert!(BlueprintPath::new(deep.to_string_lossy()).is_ok());
        assert!(BlueprintPath::new("relative/coder").is_err());
        assert!(BlueprintPath::new("x".repeat(4097)).is_err());
        assert!(BlueprintName::new("x".repeat(129)).is_err());
    }

    #[test]
    fn labels_refuse_empty_padded_long_and_control_text() {
        assert!(StageName::new("plan it").is_ok());
        assert_eq!(
            StageName::new(""),
            Err(NameError::Empty { what: "stage name" })
        );
        let padded = StageName::new(" plan").unwrap_err();
        assert!(
            padded.to_string().contains("no space at either end"),
            "{padded}"
        );
        assert!(RegionName::new("a\tb").is_err());
        let long = StageName::new("x".repeat(129)).unwrap_err();
        assert_eq!(
            long,
            NameError::TooLong {
                what: "stage name",
                max: 128,
                len: 129,
                got: "x".repeat(129),
            }
        );
        assert!(long.to_string().contains("at most 128 bytes"));
    }

    /// Every kind of name refuses empty text before it looks at the shape.
    #[test]
    fn every_kind_of_name_refuses_empty_text() {
        let empty = |what| Err(NameError::Empty { what });
        assert_eq!(InputName::new("").map(|_| ()), empty("input name"));
        assert_eq!(ToolName::new("").map(|_| ()), empty("tool name"));
        assert_eq!(RunId::new("").map(|_| ()), empty("run id"));
        assert_eq!(MimePattern::new("").map(|_| ()), empty("mime pattern"));
        assert_eq!(WorkdirPath::new("").map(|_| ()), empty("workdir path"));
        assert_eq!(HttpUrl::new("").map(|_| ()), empty("URL"));
    }

    #[test]
    fn a_blueprint_ref_with_a_digest_still_checks_the_name() {
        let digest = "a".repeat(64);
        assert!(BlueprintRef::parse(&format!(" bad@{digest}")).is_err());
    }

    #[test]
    fn identifiers_start_with_a_letter_and_stay_plain() {
        assert!(InputName::new("focus_area-2").is_ok());
        assert!(InputName::new("_x").is_ok());
        assert!(InputName::new("2x").is_err());
        assert!(ChoiceName::new("a b").is_err());
    }

    #[test]
    fn tool_names_allow_the_provider_alphabet() {
        assert!(ToolName::new("github__search.v2").is_ok());
        assert!(ToolName::new("@all").is_err());
    }

    #[test]
    fn opaque_ids_refuse_whitespace() {
        assert!(ModelId::new("claude-sonnet-5-5").is_ok());
        assert!(ProviderName::new("open router").is_err());
    }

    #[test]
    fn a_reference_with_no_provider_names_the_empty_one() {
        assert_eq!(ModelRef::parse("p/m").unwrap().provider_or_empty(), "p");
        assert_eq!(ModelRef::parse("m").unwrap().provider_or_empty(), "");
        assert!(ModelRef::parse("p/has space").is_err());
    }

    #[test]
    fn run_ids_are_safe_path_components() {
        assert!(RunId::new("coder-20260929-abc").is_ok());
        assert!(RunId::new(".dotted").is_ok());
        for bad in ["../x", "a/b", "..", "a b"] {
            let err = RunId::new(bad).unwrap_err();
            assert!(err.to_string().contains("is not `.` or `..`"), "{err}");
        }
    }

    #[test]
    fn digests_are_sha256_hex() {
        let d = Digest::of(b"hello");
        assert_eq!(
            d.as_str(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(Digest::new(d.as_str().to_uppercase()).is_err());
        assert!(Digest::new("abc").is_err());
    }

    #[test]
    fn names_serialize_as_text_and_check_on_the_way_in() {
        let name = ToolName::new("read_file").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"read_file\"");
        let err = serde_json::from_str::<ToolName>("\"no spaces\"").unwrap_err();
        assert!(err.to_string().contains("uses only letters"), "{err}");
        let back: ToolName = postcard::from_bytes(&postcard::to_stdvec(&name).unwrap()).unwrap();
        assert_eq!(back, name);
        let text: String = name.clone().into();
        assert_eq!(text, "read_file");
        assert_eq!(name.as_ref() as &str, "read_file");
        assert_eq!(format!("{name:?}"), "ToolName(\"read_file\")");
        assert_eq!("read_file".parse::<ToolName>().unwrap(), name);
        let set: std::collections::BTreeSet<ToolName> = [name].into();
        assert!(set.contains("read_file"));
    }

    #[test]
    fn mime_patterns_have_two_halves() {
        assert!(MimePattern::new("image/*").is_ok());
        assert!(MimePattern::new("*/*").is_ok());
        assert!(MimePattern::new("application/vnd.api+json").is_ok());
        assert!(MimePattern::new("image").is_err());
        assert!(MimePattern::new("image/").is_err());
    }

    #[test]
    fn workdir_paths_stay_inside() {
        assert!(WorkdirPath::new("src/main.rs").is_ok());
        for bad in ["/etc/passwd", "../up", "a//b", "a\\b", "C:/x", "a/../b"] {
            assert!(WorkdirPath::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_need_a_scheme_and_a_host() {
        assert!(HttpUrl::new("https://example.com/hook?x=1").is_ok());
        assert!(HttpUrl::new("http://localhost:8080").is_ok());
        for bad in ["ftp://x", "https://", "https:///path", "https://a b"] {
            assert!(HttpUrl::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn model_refs_split_on_the_first_slash() {
        let r = ModelRef::parse("openrouter/anthropic/claude").unwrap();
        assert_eq!(
            r.provider.as_ref().map(ProviderName::as_str),
            Some("openrouter")
        );
        assert_eq!(r.model.as_str(), "anthropic/claude");
        assert_eq!(r.to_string(), "openrouter/anthropic/claude");
        let bare = ModelRef::parse("gpt-mock").unwrap();
        assert_eq!(bare.provider, None);
        assert_eq!(bare.to_string(), "gpt-mock");
        assert!(ModelRef::parse("/x").is_err());
    }

    #[test]
    fn blueprint_refs_pin_with_an_at_sign() {
        let d = Digest::of(b"bp");
        let r = BlueprintRef::parse(&format!("coder@{d}")).unwrap();
        assert_eq!(r.digest, Some(d.clone()));
        assert_eq!(r.to_string(), format!("coder@{d}"));
        let plain = BlueprintRef::parse("coder").unwrap();
        assert_eq!(plain.to_string(), "coder");
        assert!(BlueprintRef::parse("coder@nothex").is_err());
    }
}
