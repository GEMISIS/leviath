//! A part: one typed piece of content, and the reference a stored one carries.

use serde::{Deserialize, Serialize};

use super::registry::MimeRegistry;
use super::{MimeType, text_plain};

/// How a stored part should reach a model, overriding the registry's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// As the provider's native block for its family, when the model takes it.
    Native,
    /// As text, decoded from UTF-8, whatever the model declares.
    Text,
    /// Only the stand-in, never the bytes.
    StandIn,
}

impl Delivery {
    /// The word a tool argument, an `--attach` segment or an upload request
    /// uses: `native`, `text` or `stand_in`.
    pub fn from_arg(word: &str) -> Result<Self, String> {
        match word {
            "native" => Ok(Self::Native),
            "text" => Ok(Self::Text),
            "stand_in" => Ok(Self::StandIn),
            other => Err(format!(
                "'deliver' must be native, text or stand_in, not '{other}'"
            )),
        }
    }
}

/// What a stored part carries instead of its bytes.
///
/// Small and serialisable, so a journal record, a snapshot or an event holds
/// this while the file sits once in the run's blob store under `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lowercase hex SHA-256 of the bytes; the store's key.
    pub sha256: String,
    /// The type the bytes were stored as.
    pub mime_type: MimeType,
    /// Size in bytes.
    pub size: u64,
    /// Pixel width, when a probe could read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Pixel height, when a probe could read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Duration in milliseconds, when a probe could read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The registry's token estimate at ingest, charged to the region.
    #[serde(default)]
    pub tokens: usize,
    /// The text a consumer that cannot take the part sees, rendered by the
    /// registry at ingest so nothing downstream needs the registry to read
    /// an entry: `[image/png 1024x768, 240 KB] hero.png`.
    #[serde(default)]
    pub stand_in: String,
}

impl BlobRef {
    /// Width and height together, when both are known.
    pub fn dims(&self) -> Option<(u32, u32)> {
        Some((self.width?, self.height?))
    }

    /// A short prefix of the hash, enough to name it in a line of text.
    pub fn short_sha(&self) -> &str {
        self.sha256.get(..12).unwrap_or(&self.sha256)
    }
}

/// Bytes with a type, in flight. Never serialised: a `Blob` becomes a
/// [`BlobRef`] the moment it is stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Blob {
    /// The type the bytes are.
    pub mime_type: MimeType,
    /// The bytes.
    pub bytes: Vec<u8>,
    /// A file name to show and to resolve `@name` references by.
    pub name: Option<String>,
}

impl std::fmt::Debug for Blob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blob")
            .field("mime_type", &self.mime_type)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("name", &self.name)
            .finish()
    }
}

impl Blob {
    /// A blob of `bytes` typed `mime_type`.
    pub fn new(mime_type: MimeType, bytes: Vec<u8>) -> Self {
        Self {
            mime_type,
            bytes,
            name: None,
        }
    }

    /// The same blob, named.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The reference this blob would be stored under: hash, probes, estimate,
    /// and the stand-in text rendered from the registry's row.
    pub fn describe(&self, reg: &MimeRegistry) -> BlobRef {
        let info = reg.info(&self.mime_type);
        let dims = super::probe::dimensions(&self.mime_type, &self.bytes);
        let duration_ms = super::probe::duration_ms(&self.mime_type, &self.bytes);
        let pages = super::probe::pages(&self.mime_type, &self.bytes);
        let size = self.bytes.len() as u64;
        BlobRef {
            sha256: super::store::sha256_hex(&self.bytes),
            mime_type: self.mime_type.clone(),
            size,
            width: dims.map(|d| d.0),
            height: dims.map(|d| d.1),
            duration_ms,
            tokens: info.tokens.estimate(size, dims, duration_ms, pages),
            stand_in: info.render_stand_in(self.name.as_deref(), size, dims, duration_ms, pages),
        }
    }
}

/// Where a part's bytes are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PartBody {
    /// UTF-8 text carried in the entry itself.
    Inline(String),
    /// Bytes in the run's blob store, by reference.
    Stored(BlobRef),
}

/// One typed piece of content.
///
/// A region entry, a tool output, a user message, a model reply and a final
/// output are each a list of these. Text is a part like any other; its body is
/// inline because its registry row says it is text, and that is the whole
/// difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// The part's type.
    pub mime_type: MimeType,
    /// Its bytes, inline or by reference.
    pub body: PartBody,
    /// A name: the file it came from, the artifact it is, what `@name` finds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How a stored part reaches a model, when the default is not wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<Delivery>,
}

impl Part {
    /// A `text/plain` part.
    pub fn text(s: impl Into<String>) -> Self {
        Self::inline(text_plain(), s)
    }

    /// An inline part of any text-family type.
    pub fn inline(mime_type: MimeType, s: impl Into<String>) -> Self {
        Self {
            mime_type,
            body: PartBody::Inline(s.into()),
            name: None,
            deliver: None,
        }
    }

    /// A stored part from its reference.
    pub fn stored(blob: BlobRef) -> Self {
        Self {
            mime_type: blob.mime_type.clone(),
            body: PartBody::Stored(blob),
            name: None,
            deliver: None,
        }
    }

    /// The same part, named.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The same part, with a delivery override.
    pub fn delivered(mut self, deliver: Delivery) -> Self {
        self.deliver = Some(deliver);
        self
    }

    /// The inline text, if this part carries any.
    pub fn inline_text(&self) -> Option<&str> {
        match &self.body {
            PartBody::Inline(s) => Some(s),
            PartBody::Stored(_) => None,
        }
    }

    /// The reference, if this part is stored.
    pub fn blob(&self) -> Option<&BlobRef> {
        match &self.body {
            PartBody::Inline(_) => None,
            PartBody::Stored(b) => Some(b),
        }
    }

    /// Whether this part's bytes are in the store rather than inline.
    pub fn is_stored(&self) -> bool {
        matches!(self.body, PartBody::Stored(_))
    }

    /// Whether `needle`, as a model or a person would type it, names this
    /// stored part: its exact name, the file name of a path-shaped needle
    /// (`out/hero.png` for `hero.png`), or at least six characters of its
    /// hash in either case. One rule for every tool that takes a part by
    /// name, so `context_export` and `submit_output` cannot disagree about
    /// which part `hero` means. An inline part is never named.
    pub fn is_named(&self, needle: &str) -> bool {
        let Some(blob) = self.blob() else {
            return false;
        };
        if self.name.as_deref() == Some(needle) {
            return true;
        }
        let basename = std::path::Path::new(needle)
            .file_name()
            .and_then(|n| n.to_str());
        if basename.is_some_and(|b| b != needle && self.name.as_deref() == Some(b)) {
            return true;
        }
        let lower = needle.to_ascii_lowercase();
        lower.len() >= 6 && blob.sha256.starts_with(&lower)
    }

    /// What this part costs a region.
    ///
    /// A stored part is charged its stand-in, not its native token estimate.
    /// In a region a stored blob is only a reference plus the short stand-in
    /// line that stands for it in the assembled prompt; the native estimate
    /// on the blob ref (a per-byte rule over a multi-megabyte model is
    /// millions of tokens) is the cost of sending the bytes to a model that
    /// can read them, charged at request-build time. Charged here instead, no
    /// reasonably sized region could hold real media at all.
    pub fn tokens(&self, reg: &MimeRegistry) -> usize {
        match &self.body {
            PartBody::Inline(s) => {
                let info = reg.info(&self.mime_type);
                match info.tokens {
                    super::TokenRule::PerByte(rate) if (rate - 0.25).abs() < f64::EPSILON => {
                        crate::text::estimate_tokens(s)
                    }
                    rule => rule.estimate(s.len() as u64, None, None, None),
                }
            }
            PartBody::Stored(b) => crate::text::estimate_tokens(&b.stand_in),
        }
    }

    /// The text a consumer that cannot take a stored part sees, as rendered
    /// at ingest. An inline part's stand-in is its own text.
    pub fn stand_in(&self) -> String {
        match &self.body {
            PartBody::Inline(s) => s.clone(),
            PartBody::Stored(b) => b.stand_in.clone(),
        }
    }

    /// The name this part answers to for an `@name` or tool-argument lookup:
    /// its `name`, else its short hash for a stored part.
    pub fn handle(&self) -> Option<String> {
        self.name
            .clone()
            .or_else(|| self.blob().map(|b| b.short_sha().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_word_parses_or_names_the_three_words() {
        assert_eq!(Delivery::from_arg("native"), Ok(Delivery::Native));
        assert_eq!(Delivery::from_arg("text"), Ok(Delivery::Text));
        assert_eq!(Delivery::from_arg("stand_in"), Ok(Delivery::StandIn));
        let err = Delivery::from_arg("loud").unwrap_err();
        assert!(
            err.contains("native, text or stand_in, not 'loud'"),
            "{err}"
        );
    }

    /// The one naming rule: exact name, the file name of a path, or six or
    /// more hash characters in either case. An inline part is never named.
    #[test]
    fn a_part_is_named_by_its_name_its_file_name_or_its_hash() {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(mt("image/png"), b"\x89PNG\r\n\x1a\nbytes".to_vec()).describe(&reg);
        let sha = blob.sha256.clone();
        let part = Part::stored(blob).named("hero.png");
        assert!(part.is_named("hero.png"));
        assert!(part.is_named("out/hero.png"));
        assert!(!part.is_named("hero"));
        assert!(part.is_named(&sha.get(..6).unwrap().to_ascii_uppercase()));
        assert!(!part.is_named(sha.get(..5).unwrap()));
        assert!(!Part::text("hero.png").is_named("hero.png"));
    }

    fn mt(s: &str) -> MimeType {
        MimeType::parse(s).unwrap()
    }

    #[test]
    fn text_parts_are_inline_text_plain() {
        let p = Part::text("hello");
        assert_eq!(p.mime_type.as_str(), "text/plain");
        assert_eq!(p.inline_text(), Some("hello"));
        assert!(p.blob().is_none());
        assert!(!p.is_stored());
        let reg = MimeRegistry::builtin();
        assert_eq!(p.tokens(&reg), 2);
        assert_eq!(p.stand_in(), "hello");
        assert!(p.handle().is_none());
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"mime_type":"text/plain","body":"hello"}"#);
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn inline_parts_of_other_text_types() {
        let reg = MimeRegistry::builtin();
        let p = Part::inline(mt("model/obj"), "v 1 2 3\n").named("cube.obj");
        assert_eq!(p.handle().as_deref(), Some("cube.obj"));
        assert_eq!(p.tokens(&reg), 2);
        let mut reg2 = MimeRegistry::empty();
        let t: toml::Table =
            toml::from_str("[\"x/fixed\"]\ntext = true\ntokens = { fixed = 9 }").unwrap();
        reg2.layer(&t, "t").unwrap();
        let q = Part::inline(mt("x/fixed"), "anything");
        assert_eq!(q.tokens(&reg2), 9);
    }

    #[test]
    fn stored_parts_carry_their_reference() {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(mt("image/png"), b"\x89PNG\r\n\x1a\nxxxx".to_vec()).named("a.png");
        assert!(format!("{blob:?}").contains("12 bytes"));
        let r = blob.describe(&reg);
        assert_eq!(r.size, 12);
        assert_eq!(r.sha256.len(), 64);
        assert_eq!(r.short_sha().len(), 12);
        assert!(r.dims().is_none());
        assert_eq!(r.tokens, 1600);
        let p = Part::stored(r.clone())
            .named("a.png")
            .delivered(Delivery::Text);
        assert!(p.is_stored());
        assert_eq!(p.blob(), Some(&r));
        assert!(p.inline_text().is_none());
        // A stored part is charged its stand-in against a region, not its native
        // token estimate (1600): in a region it is only a ref plus the short
        // stand-in line, and the native cost is charged at request-build time.
        assert_eq!(p.tokens(&reg), crate::text::estimate_tokens(&r.stand_in));
        assert!(p.tokens(&reg) < r.tokens);
        assert_eq!(p.stand_in(), "[image/png, 12 B] a.png");
        assert_eq!(r.stand_in, "[image/png, 12 B] a.png");
        assert_eq!(p.deliver, Some(Delivery::Text));
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"deliver\":\"text\""));
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        let unnamed = Part::stored(r.clone());
        assert_eq!(unnamed.handle().unwrap(), r.short_sha());
        let short = BlobRef {
            sha256: "abc".into(),
            width: Some(4),
            ..r
        };
        assert_eq!(short.short_sha(), "abc");
        assert_eq!(short.dims(), None, "a width without a height is not a size");
    }

    #[test]
    fn dims_and_duration_flow_through() {
        let reg = MimeRegistry::builtin();
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        png.extend_from_slice(&[0, 0, 0x04, 0x00, 0, 0, 0x03, 0x00]);
        let r = Blob::new(mt("image/png"), png)
            .named("hero.png")
            .describe(&reg);
        assert_eq!(r.dims(), Some((1024, 768)));
        assert_eq!(r.tokens, 1049);
        let p = Part::stored(r).named("hero.png");
        assert_eq!(p.stand_in(), "[image/png 1024x768, 24 B] hero.png");
        let unnamed = Blob::new(mt("audio/wav"), vec![1, 2, 3]).describe(&reg);
        // A document is billed by its pages, not its bytes: a 2 MB brochure
        // was charged over a million tokens under a per-byte rule and could
        // not be sent to any model.
        let mut pdf =
            b"%PDF-1.7\n<< /Type /Pages /Count 2 >> << /Type /Page >> << /Type /Page >>".to_vec();
        pdf.extend(std::iter::repeat_n(b' ', 2 * 1024 * 1024));
        let brochure = Blob::new(mt("application/pdf"), pdf)
            .named("brochure.pdf")
            .describe(&reg);
        assert_eq!(brochure.tokens, 4000);
        assert!(
            brochure.stand_in.contains("2 pages"),
            "{}",
            brochure.stand_in
        );
        assert!(
            brochure.stand_in.contains("brochure.pdf"),
            "{}",
            brochure.stand_in
        );
        assert_eq!(Part::stored(unnamed).stand_in(), "[audio/wav, 3 B]");
    }

    #[test]
    fn delivery_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&Delivery::StandIn).unwrap(),
            "\"stand_in\""
        );
        assert_eq!(
            serde_json::from_str::<Delivery>("\"native\"").unwrap(),
            Delivery::Native
        );
    }
}
