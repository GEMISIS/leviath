//! The table that says what a mime type is.
//!
//! The engine never asks "is this an image". It asks the registry for a
//! type's [`MimeInfo`]: which family it belongs to (so a provider can pick an
//! encoder), whether its bytes are text (so they can travel inline), how to
//! estimate its tokens, which extensions imply it, and what to show a consumer
//! that cannot take it. Rows come from two places and later ones win field by
//! field: the defaults compiled into this crate, then `[mime_types]` in the
//! user's config.
//!
//! Resolution is layered too. `image/png` inherits every field it does not
//! set from `image/*`, which inherits from `*/*`, so a user row can add one
//! extension without restating the family.
//!
//! A row may also name a `check`: something that looks at bytes claiming
//! the type and refuses the ones that are not what they say. The row only
//! carries the name (a script path, as written); whoever builds the
//! registry compiles it and [`attaches`](MimeRegistry::attach_check) the
//! result, and [`verify`](MimeRegistry::verify) runs it wherever bytes are
//! stored.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::MimeType;
use super::check::MimeCheck;

/// The rows compiled into this crate.
const DEFAULTS: &str = include_str!("../../mime/defaults.toml");

/// How to estimate a part's token cost before a provider bills it.
///
/// The number is charged to the region the part lands in, so a budget can
/// hold an image to the same rule it holds text to. Providers correct it after
/// the first billed call, the same calibration text estimates get.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "TokenRuleWire", into = "TokenRuleWire")]
pub enum TokenRule {
    /// Tokens per byte of the stored file. `0.25` is the text rule.
    PerByte(f64),
    /// Pixels per token, capped. Needs dimensions; falls back to `max`.
    PerPixel {
        /// How many pixels one token buys.
        divisor: u32,
        /// The most one part can cost, and the answer when dimensions are unknown.
        max: usize,
    },
    /// Tokens per second of mime. Needs a duration; falls back to a byte guess.
    PerSecond(u32),
    /// Tokens per page of a document. Needs a page count; without one, a
    /// page is assumed every [`PAGE_BYTES_GUESS`] bytes.
    PerPage(usize),
    /// A flat charge whatever the size.
    Fixed(usize),
}

/// The bytes one page is taken to hold when a document's pages cannot be
/// counted. A text page is a few kilobytes and a scanned one a few hundred, so
/// 64 KiB errs high for text, the safe side for a budget, and lands near a
/// slide deck or a scan.
pub const PAGE_BYTES_GUESS: u64 = 64 * 1024;

/// The flat table a rule is written as: exactly one of `per_byte`,
/// `per_pixel` (with `max`), `per_second` or `fixed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenRuleWire {
    /// See [`TokenRule::PerByte`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_byte: Option<f64>,
    /// See [`TokenRule::PerPixel`]: the divisor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_pixel: Option<u32>,
    /// See [`TokenRule::PerPixel`]: the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
    /// See [`TokenRule::PerSecond`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_second: Option<u32>,
    /// See [`TokenRule::PerPage`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_page: Option<usize>,
    /// See [`TokenRule::Fixed`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<usize>,
}

impl TryFrom<TokenRuleWire> for TokenRule {
    type Error = String;

    fn try_from(w: TokenRuleWire) -> Result<Self, Self::Error> {
        if w.max.is_some() && w.per_pixel.is_none() {
            return Err("tokens.max only goes with per_pixel".to_string());
        }
        match (w.per_byte, w.per_pixel, w.per_second, w.per_page, w.fixed) {
            (Some(rate), None, None, None, None) => Ok(TokenRule::PerByte(rate)),
            (None, Some(divisor), None, None, None) => Ok(TokenRule::PerPixel {
                divisor,
                max: w.max.unwrap_or(1600),
            }),
            (None, None, Some(rate), None, None) => Ok(TokenRule::PerSecond(rate)),
            (None, None, None, Some(rate), None) => Ok(TokenRule::PerPage(rate)),
            (None, None, None, None, Some(n)) => Ok(TokenRule::Fixed(n)),
            _ => Err(
                "tokens needs exactly one of per_byte, per_pixel, per_second, per_page, fixed"
                    .to_string(),
            ),
        }
    }
}

impl From<TokenRule> for TokenRuleWire {
    fn from(rule: TokenRule) -> Self {
        let mut w = TokenRuleWire::default();
        match rule {
            TokenRule::PerByte(rate) => w.per_byte = Some(rate),
            TokenRule::PerPixel { divisor, max } => {
                w.per_pixel = Some(divisor);
                w.max = Some(max);
            }
            TokenRule::PerSecond(rate) => w.per_second = Some(rate),
            TokenRule::PerPage(rate) => w.per_page = Some(rate),
            TokenRule::Fixed(n) => w.fixed = Some(n),
        }
        w
    }
}

impl TokenRule {
    /// The estimate for a part of `size` bytes with what is known about it.
    pub fn estimate(
        self,
        size: u64,
        dims: Option<(u32, u32)>,
        duration_ms: Option<u64>,
        pages: Option<u32>,
    ) -> usize {
        let per_byte = |rate: f64| ((size as f64) * rate).ceil() as usize;
        match self {
            TokenRule::PerByte(rate) => per_byte(rate).max(1),
            TokenRule::PerPixel { divisor, max } => match dims {
                Some((w, h)) => {
                    let pixels = u64::from(w) * u64::from(h);
                    (pixels.div_ceil(u64::from(divisor.max(1))) as usize).clamp(1, max.max(1))
                }
                None => max.max(1),
            },
            TokenRule::PerSecond(rate) => match duration_ms {
                Some(ms) => (ms.div_ceil(1000) as usize * rate as usize).max(1),
                // No duration: about a token per 32 bytes of compressed audio,
                // which is what a minute of 128 kbps MP3 costs under the
                // per-second rule.
                None => per_byte(1.0 / 32.0).max(1),
            },
            TokenRule::PerPage(rate) => {
                let pages =
                    pages.map_or_else(|| size.div_ceil(PAGE_BYTES_GUESS) as usize, |n| n as usize);
                pages.saturating_mul(rate).max(1)
            }
            TokenRule::Fixed(n) => n,
        }
    }
}

/// One row as written: every field optional, so a row names only what it sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MimeRow {
    /// See [`MimeInfo::family`].
    pub family: Option<String>,
    /// See [`MimeInfo::text`].
    pub text: Option<bool>,
    /// See [`MimeInfo::tokens`].
    pub tokens: Option<TokenRule>,
    /// See [`MimeInfo::extensions`].
    pub extensions: Option<Vec<String>>,
    /// A hex prefix identifying the bytes.
    pub magic: Option<String>,
    /// See [`MimeInfo::stand_in`].
    pub stand_in: Option<String>,
    /// See [`MimeInfo::check`]. An empty string lifts a check a broader row
    /// put on the type.
    pub check: Option<String>,
}

/// Everything the engine wants to know about one mime type, resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MimeInfo {
    /// The type this answers for.
    pub mime_type: MimeType,
    /// What providers key their encoders on. Core passes it through.
    pub family: String,
    /// Whether the bytes are UTF-8 text that may travel inline and reach a
    /// text model directly.
    pub text: bool,
    /// How to estimate the part's tokens.
    pub tokens: TokenRule,
    /// File extensions, lowercase, without the dot.
    pub extensions: Vec<String>,
    /// The stand-in template, when a row set one. See [`MimeInfo::render_stand_in`].
    pub stand_in: Option<String>,
    /// The check the bytes must pass to be stored as this type, as the row
    /// wrote it (a script path). `None` when no row puts one on the type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Where the most specific row came from.
    pub source: String,
}

impl MimeInfo {
    /// The text a consumer that cannot take this type sees in the part's place.
    ///
    /// The default is `[image/png 1024x768, 240 KB] hero.png`. A row's
    /// `stand_in` template may use `{type}`, `{name}`, `{size}`, `{dims}` and
    /// `{duration}`; an unknown dimension or duration renders empty.
    pub fn render_stand_in(
        &self,
        name: Option<&str>,
        size: u64,
        dims: Option<(u32, u32)>,
        duration_ms: Option<u64>,
        pages: Option<u32>,
    ) -> String {
        let dims_s = dims.map(|(w, h)| format!("{w}x{h}")).unwrap_or_default();
        let dur_s = duration_ms
            .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
            .unwrap_or_default();
        let pages_s = pages
            .map(|n| match n {
                1 => "1 page".to_string(),
                n => format!("{n} pages"),
            })
            .unwrap_or_default();
        let size_s = super::human_size(size);
        let name_s = name.unwrap_or("");
        match &self.stand_in {
            Some(t) => t
                .replace("{type}", self.mime_type.as_str())
                .replace("{name}", name_s)
                .replace("{size}", &size_s)
                .replace("{dims}", &dims_s)
                .replace("{duration}", &dur_s)
                .replace("{pages}", &pages_s),
            None => {
                let mut s = format!("[{}", self.mime_type);
                if !dims_s.is_empty() {
                    s.push(' ');
                    s.push_str(&dims_s);
                } else if !dur_s.is_empty() {
                    s.push(' ');
                    s.push_str(&dur_s);
                } else if !pages_s.is_empty() {
                    s.push(' ');
                    s.push_str(&pages_s);
                }
                s.push_str(", ");
                s.push_str(&size_s);
                s.push(']');
                if !name_s.is_empty() {
                    s.push(' ');
                    s.push_str(name_s);
                }
                s
            }
        }
    }
}

/// A row plus where it came from.
#[derive(Debug, Clone)]
struct Layered {
    row: MimeRow,
    source: String,
}

/// The registry: rows keyed by type or pattern, resolved by layering.
#[derive(Debug, Clone)]
pub struct MimeRegistry {
    rows: BTreeMap<String, Layered>,
    /// The compiled checks, keyed like the rows that name them.
    checks: BTreeMap<String, Arc<dyn MimeCheck>>,
}

/// Why a `[mime_types]` table was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RegistryError {
    /// A key that is not `type/subtype` or `type/*`.
    #[error("mime_types key {0}")]
    Key(String),
    /// A row whose fields do not deserialise.
    #[error("mime_types.\"{key}\": {message}")]
    Row {
        /// The offending key.
        key: String,
        /// What was wrong with it.
        message: String,
    },
    /// A `magic` value that is not hex.
    #[error("mime_types.\"{0}\": magic must be hex digits")]
    Magic(String),
}

impl Default for MimeRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl MimeRegistry {
    /// An empty registry, for tests that want to see layering from nothing.
    pub fn empty() -> Self {
        Self {
            rows: BTreeMap::new(),
            checks: BTreeMap::new(),
        }
    }

    /// The registry with only the compiled-in rows.
    ///
    /// The compiled table is checked by a test, so a failure here can only be
    /// a build with a broken defaults file; it degrades to the empty registry,
    /// whose `*/*` floor still answers every question.
    pub fn builtin() -> Self {
        Self::from_defaults(DEFAULTS)
    }

    /// [`Self::builtin`], reporting why the compiled rows would not load.
    pub fn builtin_checked() -> Result<Self, RegistryError> {
        Self::from_toml_str(DEFAULTS, "builtin")
    }

    /// A registry from a defaults document, or the empty one when it will
    /// not load.
    fn from_defaults(text: &str) -> Self {
        Self::from_toml_str(text, "builtin").unwrap_or_else(|_| Self::empty())
    }

    /// A registry from the text of a `[mime_types]`-shaped document.
    fn from_toml_str(text: &str, source: &str) -> Result<Self, RegistryError> {
        let table: toml::Table = toml::from_str(text).map_err(|e| RegistryError::Row {
            key: source.to_string(),
            message: e.message().to_string(),
        })?;
        let mut reg = Self::empty();
        reg.layer(&table, source)?;
        Ok(reg)
    }

    /// Overlay every row in `table`, a `[mime_types]` body whose keys are
    /// types or patterns, naming `source` as where they came from.
    pub fn layer(&mut self, table: &toml::Table, source: &str) -> Result<(), RegistryError> {
        for (key, value) in table {
            let key_norm = normalise_key(key).ok_or_else(|| RegistryError::Key(key.clone()))?;
            let row: MimeRow =
                value
                    .clone()
                    .try_into()
                    .map_err(|e: toml::de::Error| RegistryError::Row {
                        key: key.clone(),
                        message: e.message().to_string(),
                    })?;
            if row
                .magic
                .as_deref()
                .is_some_and(|m| decode_magic(m).is_none())
            {
                return Err(RegistryError::Magic(key.clone()));
            }
            // A row naming a check (or lifting one) replaces whatever was
            // compiled for the key under it; the new name is compiled and
            // attached by whoever is layering.
            if row.check.is_some() {
                self.checks.remove(&key_norm);
            }
            let merged = match self.rows.remove(&key_norm) {
                Some(existing) => merge_rows(existing.row, row),
                None => row,
            };
            self.rows.insert(
                key_norm,
                Layered {
                    row: merged,
                    source: source.to_string(),
                },
            );
        }
        Ok(())
    }

    /// This registry with `table` layered on top, as a new value.
    pub fn layered(&self, table: &toml::Table, source: &str) -> Result<Self, RegistryError> {
        let mut next = self.clone();
        next.layer(table, source)?;
        Ok(next)
    }

    /// Put a compiled check on `key`, the type or pattern of the row that
    /// named it. The key is refused if it is not a type or a pattern.
    pub fn attach_check(
        &mut self,
        key: &str,
        check: Arc<dyn MimeCheck>,
    ) -> Result<(), RegistryError> {
        let key_norm = normalise_key(key).ok_or_else(|| RegistryError::Key(key.to_string()))?;
        self.checks.insert(key_norm, check);
        Ok(())
    }

    /// Every row that names a check, as `(key, check, source)`: what a
    /// builder has to compile and attach, in key order.
    pub fn declared_checks(&self) -> Vec<(String, String, String)> {
        self.rows
            .iter()
            .filter_map(|(key, l)| {
                l.row
                    .check
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .map(|c| (key.clone(), c.to_string(), l.source.clone()))
            })
            .collect()
    }

    /// The compiled check `mime_type` answers to: its own row's, else its
    /// family's, else the floor's, stopping at a row that lifts the check
    /// with an empty name. `None` when no row names one, or the row that
    /// does has had nothing attached for it.
    pub fn check_for(&self, mime_type: &MimeType) -> Option<&Arc<dyn MimeCheck>> {
        let chain = [
            mime_type.as_str().to_string(),
            mime_type.family_pattern(),
            "*/*".to_string(),
        ];
        for key in chain {
            let Some(l) = self.rows.get(&key) else {
                continue;
            };
            match l.row.check.as_deref() {
                Some(c) if c.trim().is_empty() => return None,
                Some(_) => return self.checks.get(&key),
                None => {}
            }
        }
        None
    }

    /// Run the check for `mime_type` over `bytes`, if a row put one on the
    /// type. Bytes that fail come back with the reason; a type with no check
    /// passes.
    pub fn verify(&self, mime_type: &MimeType, bytes: &[u8]) -> Result<(), String> {
        match self.check_for(mime_type) {
            Some(check) => check.check(mime_type, bytes),
            None => Ok(()),
        }
    }

    /// Every key this registry holds, with its source, in key order.
    pub fn keys(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|(k, v)| (k.clone(), v.source.clone()))
            .collect()
    }

    /// The row as written for `key`, if any.
    pub fn row(&self, key: &str) -> Option<&MimeRow> {
        normalise_key(key)
            .and_then(|k| self.rows.get(&k))
            .map(|l| &l.row)
    }

    /// The resolved information for `mime_type`. Always answers: the `*/*`
    /// row is the floor, and a registry without one falls back to the
    /// compiled-in binary defaults.
    pub fn info(&self, mime_type: &MimeType) -> MimeInfo {
        let mut family = "binary".to_string();
        let mut text = false;
        let mut tokens = TokenRule::PerByte(0.34);
        let mut extensions = Vec::new();
        let mut stand_in = None;
        let mut check = None;
        let mut source = "fallback".to_string();
        let chain = [
            "*/*".to_string(),
            mime_type.family_pattern(),
            mime_type.as_str().to_string(),
        ];
        for key in chain {
            if let Some(l) = self.rows.get(&key) {
                if let Some(f) = &l.row.family {
                    family = f.clone();
                }
                if let Some(t) = l.row.text {
                    text = t;
                }
                if let Some(r) = l.row.tokens {
                    tokens = r;
                }
                if let Some(e) = &l.row.extensions {
                    extensions = e.iter().map(|s| s.to_ascii_lowercase()).collect();
                }
                if let Some(s) = &l.row.stand_in {
                    stand_in = Some(s.clone());
                }
                if let Some(c) = &l.row.check {
                    check = Some(c.clone()).filter(|c| !c.trim().is_empty());
                }
                source = l.source.clone();
            }
        }
        MimeInfo {
            mime_type: mime_type.clone(),
            family,
            text,
            tokens,
            extensions,
            stand_in,
            check,
            source,
        }
    }

    /// The type a file extension implies, if a row claims it.
    ///
    /// The extension may carry its dot and any case. When two rows claim one
    /// extension the earlier key wins, which is deterministic and documented
    /// rather than clever.
    pub fn from_extension(&self, ext: &str) -> Option<MimeType> {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() {
            return None;
        }
        self.rows.iter().find_map(|(key, l)| {
            let claims = l
                .row
                .extensions
                .as_ref()
                .is_some_and(|exts| exts.iter().any(|e| e.eq_ignore_ascii_case(&ext)));
            (claims && !key.ends_with("/*"))
                .then(|| MimeType::parse(key).ok())
                .flatten()
        })
    }

    /// The type a file name implies, from its extension.
    pub fn from_name(&self, name: &str) -> Option<MimeType> {
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())?;
        self.from_extension(ext)
    }

    /// The type the leading bytes identify, if a row's `magic` matches.
    /// The longest matching prefix wins, so a RIFF container with a `WEBP`
    /// tag beats a row that only knows `RIFF`.
    pub fn sniff(&self, bytes: &[u8]) -> Option<MimeType> {
        let mut best: Option<(usize, MimeType)> = None;
        for (key, l) in &self.rows {
            let Some(prefix) = l.row.magic.as_deref().and_then(decode_magic) else {
                continue;
            };
            let matches = bytes.len() >= prefix.len()
                && prefix
                    .iter()
                    .zip(bytes)
                    .all(|(want, have)| want.is_none_or(|w| w == *have));
            if matches
                && best.as_ref().is_none_or(|(len, _)| prefix.len() > *len)
                && let Ok(t) = MimeType::parse(key)
            {
                best = Some((prefix.len(), t));
            }
        }
        best.map(|(_, t)| t)
    }

    /// Decide a type for bytes that arrived with `declared` type and `name`.
    ///
    /// A declared type wins. Otherwise the bytes are sniffed, then the name's
    /// extension is consulted, then valid UTF-8 counts as `text/plain`, and
    /// anything else is `application/octet-stream`.
    pub fn resolve(
        &self,
        declared: Option<&MimeType>,
        name: Option<&str>,
        bytes: &[u8],
    ) -> MimeType {
        if let Some(t) = declared {
            return t.clone();
        }
        if let Some(t) = self.sniff(bytes) {
            return t;
        }
        if let Some(t) = name.and_then(|n| self.from_name(n)) {
            return t;
        }
        if std::str::from_utf8(bytes).is_ok() {
            return super::text_plain();
        }
        super::octet_stream()
    }
}

/// `image/png` or `image/*`, lowercase, or `None` for anything else.
fn normalise_key(key: &str) -> Option<String> {
    let k = key.trim().to_ascii_lowercase();
    let (kind, sub) = k.split_once('/')?;
    if kind.is_empty() || sub.is_empty() || sub.contains('/') {
        return None;
    }
    if sub == "*" {
        return (kind == "*" || MimeType::parse(&format!("{kind}/x")).is_ok()).then_some(k);
    }
    MimeType::parse(&k).ok().map(|t| t.as_str().to_string())
}

/// `over` on top of `under`, field by field.
fn merge_rows(under: MimeRow, over: MimeRow) -> MimeRow {
    MimeRow {
        family: over.family.or(under.family),
        text: over.text.or(under.text),
        tokens: over.tokens.or(under.tokens),
        extensions: over.extensions.or(under.extensions),
        magic: over.magic.or(under.magic),
        stand_in: over.stand_in.or(under.stand_in),
        check: over.check.or(under.check),
    }
}

/// A `magic` string as the bytes it matches: two hex digits per byte, or
/// `??` for a byte that may be anything, so a container format can be told
/// by a tag past its length field (`52494646????????57454250` is `RIFF`,
/// four bytes of size, `WEBP`). Empty, odd-length, or holding anything else
/// is `None`.
fn decode_magic(s: &str) -> Option<Vec<Option<u8>>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| match pair {
            b"??" => Some(None),
            _ => {
                let hi = (pair[0] as char).to_digit(16)?;
                let lo = (pair[1] as char).to_digit(16)?;
                Some(Some((hi * 16 + lo) as u8))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mt(s: &str) -> MimeType {
        MimeType::parse(s).unwrap()
    }

    #[test]
    fn defaults_parse_and_resolve() {
        MimeRegistry::builtin_checked().expect("the compiled defaults load");
        let reg = MimeRegistry::builtin();
        let png = reg.info(&mt("image/png"));
        assert_eq!(png.family, "image");
        assert!(!png.text);
        assert_eq!(png.extensions, vec!["png"]);
        assert_eq!(png.source, "builtin");
        assert_eq!(
            png.tokens,
            TokenRule::PerPixel {
                divisor: 750,
                max: 1600
            }
        );
        let obj = reg.info(&mt("model/obj"));
        assert_eq!(obj.family, "model");
        assert!(obj.text);
        let md = reg.info(&mt("text/markdown"));
        assert!(md.text);
        assert_eq!(md.family, "text");
        let unknown = reg.info(&mt("application/x-made-up"));
        assert_eq!(unknown.family, "binary");
        assert!(!unknown.text);
        assert_eq!(unknown.source, "builtin");
        let unknown_kind = reg.info(&mt("chemical/x-pdb"));
        assert_eq!(unknown_kind.family, "binary");
        assert!(!reg.keys().is_empty());
        assert!(reg.row("image/png").is_some());
        assert!(reg.row("nope").is_none());
    }

    #[test]
    fn empty_registry_has_a_floor() {
        let reg = MimeRegistry::empty();
        let info = reg.info(&mt("image/png"));
        assert_eq!(info.family, "binary");
        assert_eq!(info.source, "fallback");
        assert!(reg.from_extension("png").is_none());
        assert!(reg.sniff(b"\x89PNG").is_none());
        assert_eq!(reg.resolve(None, None, b"hello").as_str(), "text/plain");
        assert_eq!(
            reg.resolve(None, None, &[0xff, 0xfe, 0x00]).as_str(),
            "application/octet-stream"
        );
        let d: MimeRegistry = MimeRegistry::default();
        assert!(d.row("image/png").is_some());
    }

    #[test]
    fn layering_merges_field_by_field() {
        let mut reg = MimeRegistry::builtin();
        let table: toml::Table = toml::from_str(
            r#"
            ["image/png"]
            extensions = ["png", "apng"]
            ["model/obj"]
            text = false
            stand_in = "<{type} {name} {size} {dims} {duration}>"
            ["chemical/*"]
            family = "chem"
            "#,
        )
        .unwrap();
        reg.layer(&table, "config").unwrap();
        let png = reg.info(&mt("image/png"));
        assert_eq!(png.extensions, vec!["png", "apng"]);
        assert_eq!(png.family, "image");
        assert_eq!(png.source, "config");
        assert!(reg.row("image/png").unwrap().magic.is_some());
        let obj = reg.info(&mt("model/obj"));
        assert!(!obj.text);
        assert_eq!(obj.family, "model");
        assert_eq!(
            obj.render_stand_in(Some("a.obj"), 10, Some((1, 2)), Some(1500), None),
            "<model/obj a.obj 10 B 1x2 1.5s>"
        );
        assert_eq!(reg.info(&mt("chemical/x-pdb")).family, "chem");
        assert_eq!(reg.from_extension("APNG").unwrap().as_str(), "image/png");
    }

    #[test]
    fn layering_refuses_bad_rows() {
        let mut reg = MimeRegistry::empty();
        let bad_key: toml::Table = toml::from_str("[png]\nfamily = \"image\"").unwrap();
        assert_eq!(
            reg.layer(&bad_key, "t").unwrap_err(),
            RegistryError::Key("png".into())
        );
        let bad_field: toml::Table = toml::from_str("[\"image/png\"]\nfamilies = 1").unwrap();
        let err = reg.layer(&bad_field, "t").unwrap_err();
        assert!(
            err.to_string()
                .starts_with("mime_types.\"image/png\": unknown field `families`"),
            "{err}"
        );
        let not_toml = MimeRegistry::from_toml_str("not [[ toml", "t").unwrap_err();
        assert!(not_toml.to_string().starts_with("mime_types.\"t\""));
        assert_eq!(
            MimeRegistry::from_toml_str("[png]\nfamily = \"x\"", "t").unwrap_err(),
            RegistryError::Key("png".into())
        );
        assert!(MimeRegistry::from_defaults("not [[ toml").keys().is_empty());
        let bad_magic: toml::Table = toml::from_str("[\"image/png\"]\nmagic = \"zz\"").unwrap();
        assert_eq!(
            reg.layer(&bad_magic, "t").unwrap_err(),
            RegistryError::Magic("image/png".into())
        );
        let empty_magic: toml::Table = toml::from_str("[\"image/png\"]\nmagic = \"\"").unwrap();
        assert!(reg.layer(&empty_magic, "t").is_err());
        let bad_wild: toml::Table = toml::from_str("[\"a/b/*\"]\nfamily = \"x\"").unwrap();
        assert!(reg.layer(&bad_wild, "t").is_err());
        let bad_wild2: toml::Table = toml::from_str("[\"a b/*\"]\nfamily = \"x\"").unwrap();
        assert!(reg.layer(&bad_wild2, "t").is_err());
        let err = RegistryError::Row {
            key: "k".into(),
            message: "m".into(),
        };
        assert_eq!(err.to_string(), "mime_types.\"k\": m");
        assert!(RegistryError::Magic("k".into()).to_string().contains("hex"));
        assert!(RegistryError::Key("k".into()).to_string().contains("k"));
    }

    #[test]
    fn extension_and_name_lookup() {
        let reg = MimeRegistry::builtin();
        assert_eq!(reg.from_extension(".JPG").unwrap().as_str(), "image/jpeg");
        assert_eq!(reg.from_name("clip.MP4").unwrap().as_str(), "video/mp4");
        assert!(reg.from_name("Makefile").is_none());
        assert!(reg.from_extension("").is_none());
        assert!(reg.from_extension("zzz").is_none());
        assert_eq!(reg.from_extension("obj").unwrap().as_str(), "model/obj");
    }

    /// The RIFF containers open with the same four letters and are told
    /// apart by the tag after the length field, which `??` skips.
    #[test]
    fn wildcard_magic_tells_riff_containers_apart_and_a_bad_magic_is_refused() {
        let reg = MimeRegistry::builtin();
        assert_eq!(reg.sniff(b"GIF89a\x01\x00").unwrap().as_str(), "image/gif");
        assert_eq!(
            reg.sniff(b"RIFF\x24\x08\x00\x00WAVEfmt ").unwrap().as_str(),
            "audio/wav"
        );
        assert_eq!(
            reg.sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 ").unwrap().as_str(),
            "image/webp"
        );
        assert!(reg.sniff(b"RIFF\x00\x00\x00\x00AVI LIST").is_none());
        assert!(reg.sniff(b"RIFF\x00\x00").is_none());
        for bad in ["474946383", "?A", "", "GG"] {
            let mut reg = MimeRegistry::empty();
            let table: toml::Table =
                toml::from_str(&format!("[\"x/y\"]\nmagic = \"{bad}\"\n")).unwrap();
            assert!(
                matches!(reg.layer(&table, "t"), Err(RegistryError::Magic(k)) if k == "x/y"),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn sniffing_prefers_the_longest_magic() {
        let reg = MimeRegistry::builtin();
        assert_eq!(
            reg.sniff(b"\x89PNG\r\n\x1a\nrest").unwrap().as_str(),
            "image/png"
        );
        assert_eq!(
            reg.sniff(b"\xff\xd8\xff\xe0").unwrap().as_str(),
            "image/jpeg"
        );
        assert_eq!(reg.sniff(b"%PDF-1.7").unwrap().as_str(), "application/pdf");
        assert!(reg.sniff(b"\x89PN").is_none());
        assert!(reg.sniff(b"plain").is_none());
        let mut reg = MimeRegistry::empty();
        let table: toml::Table = toml::from_str(
            r#"
            ["x/short"]
            magic = "AB"
            ["x/long"]
            magic = "ABCD"
            "#,
        )
        .unwrap();
        reg.layer(&table, "t").unwrap();
        assert_eq!(reg.sniff(&[0xab, 0xcd, 0x01]).unwrap().as_str(), "x/long");
        assert_eq!(reg.sniff(&[0xab, 0x00]).unwrap().as_str(), "x/short");
        assert_eq!(decode_magic("ABC"), None);
        assert_eq!(decode_magic("GG"), None);
        assert_eq!(decode_magic("AG"), None);
        assert_eq!(decode_magic("?A"), None);
        assert_eq!(decode_magic("AB??"), Some(vec![Some(0xAB), None]));
    }

    #[test]
    fn resolve_order() {
        let reg = MimeRegistry::builtin();
        let declared = mt("model/stl");
        assert_eq!(
            reg.resolve(Some(&declared), Some("a.png"), b"\x89PNG\r\n\x1a\n"),
            declared
        );
        assert_eq!(
            reg.resolve(None, Some("a.txt"), b"\x89PNG\r\n\x1a\n")
                .as_str(),
            "image/png"
        );
        assert_eq!(
            reg.resolve(None, Some("a.md"), b"# hi").as_str(),
            "text/markdown"
        );
        assert_eq!(
            reg.resolve(None, Some("a.zzz"), b"hi").as_str(),
            "text/plain"
        );
        assert_eq!(
            reg.resolve(None, None, &[0, 159, 146, 150]).as_str(),
            "application/octet-stream"
        );
    }

    #[test]
    fn token_rules() {
        assert_eq!(TokenRule::PerByte(0.25).estimate(8, None, None, None), 2);
        assert_eq!(TokenRule::PerByte(0.25).estimate(0, None, None, None), 1);
        let px = TokenRule::PerPixel {
            divisor: 750,
            max: 1600,
        };
        assert_eq!(px.estimate(0, Some((750, 2)), None, None), 2);
        assert_eq!(px.estimate(0, Some((4000, 4000)), None, None), 1600);
        assert_eq!(px.estimate(0, None, None, None), 1600);
        assert_eq!(px.estimate(0, Some((0, 0)), None, None), 1);
        let zero_div = TokenRule::PerPixel { divisor: 0, max: 0 };
        assert_eq!(zero_div.estimate(0, Some((3, 3)), None, None), 1);
        assert_eq!(
            TokenRule::PerSecond(32).estimate(0, None, Some(1500), None),
            64
        );
        assert_eq!(TokenRule::PerSecond(32).estimate(0, None, Some(0), None), 1);
        assert_eq!(
            TokenRule::PerSecond(32).estimate(3200, None, None, None),
            100
        );
        assert_eq!(TokenRule::Fixed(7).estimate(99, None, None, None), 7);
        // Pages when counted, a page per 64 KiB when not, never nothing.
        assert_eq!(
            TokenRule::PerPage(2000).estimate(0, None, None, Some(3)),
            6000
        );
        assert_eq!(TokenRule::PerPage(2000).estimate(0, None, None, Some(0)), 1);
        assert_eq!(
            TokenRule::PerPage(2000).estimate(65_537, None, None, None),
            4000
        );
        assert_eq!(TokenRule::PerPage(2000).estimate(0, None, None, None), 1);
        let per_page: TokenRule = serde_json::from_str("{\"per_page\":2000}").unwrap();
        assert_eq!(per_page, TokenRule::PerPage(2000));
        assert_eq!(
            serde_json::to_string(&per_page).unwrap(),
            "{\"per_page\":2000}"
        );
        let two: Result<TokenRule, _> = serde_json::from_str("{\"per_page\":1,\"fixed\":2}");
        assert!(two.unwrap_err().to_string().contains("per_page"));
        let json = serde_json::to_string(&TokenRule::PerSecond(32)).unwrap();
        assert_eq!(json, "{\"per_second\":32}");
        let px: TokenRule = serde_json::from_str("{\"per_pixel\":750,\"max\":1600}").unwrap();
        assert_eq!(
            px,
            TokenRule::PerPixel {
                divisor: 750,
                max: 1600
            }
        );
        let px_default: TokenRule = serde_json::from_str("{\"per_pixel\":750}").unwrap();
        assert_eq!(
            px_default,
            TokenRule::PerPixel {
                divisor: 750,
                max: 1600
            }
        );
        assert_eq!(
            serde_json::to_string(&px).unwrap(),
            "{\"per_pixel\":750,\"max\":1600}"
        );
        assert_eq!(
            serde_json::to_string(&TokenRule::Fixed(3)).unwrap(),
            "{\"fixed\":3}"
        );
        assert_eq!(
            serde_json::to_string(&TokenRule::PerByte(0.5)).unwrap(),
            "{\"per_byte\":0.5}"
        );
        let fixed: TokenRule = serde_json::from_str("{\"fixed\":3}").unwrap();
        assert_eq!(fixed, TokenRule::Fixed(3));
        let per_byte: TokenRule = serde_json::from_str("{\"per_byte\":0.5}").unwrap();
        assert_eq!(per_byte, TokenRule::PerByte(0.5));
        let none = serde_json::from_str::<TokenRule>("{}").unwrap_err();
        assert!(none.to_string().contains("exactly one"));
        let two = serde_json::from_str::<TokenRule>("{\"fixed\":1,\"per_byte\":1.0}").unwrap_err();
        assert!(two.to_string().contains("exactly one"));
        let stray_max = serde_json::from_str::<TokenRule>("{\"fixed\":1,\"max\":2}").unwrap_err();
        assert!(stray_max.to_string().contains("max"));
        assert!(serde_json::from_str::<TokenRule>("{\"per_byte\":\"x\"}").is_err());
    }

    /// A row's `check` resolves like any other field, an empty one lifts it,
    /// and the compiled object attached under the key is what `verify` runs.
    #[test]
    fn a_check_resolves_by_row_and_runs_when_attached() {
        use crate::mime::FnCheck;
        let mut reg = MimeRegistry::builtin();
        let table: toml::Table = toml::from_str(
            "[\"image/*\"]\ncheck = \"checks/image.rhai\"\n[\"image/gif\"]\ncheck = \"\"\n",
        )
        .unwrap();
        reg.layer(&table, "config").unwrap();
        assert_eq!(
            reg.info(&mt("image/png")).check.as_deref(),
            Some("checks/image.rhai"),
            "a subtype inherits the family's check"
        );
        assert_eq!(
            reg.info(&mt("image/gif")).check,
            None,
            "an empty name lifts it"
        );
        assert_eq!(reg.info(&mt("audio/wav")).check, None);
        assert_eq!(
            reg.declared_checks(),
            vec![(
                "image/*".to_string(),
                "checks/image.rhai".to_string(),
                "config".to_string()
            )]
        );

        // Declared but nothing attached: nothing runs.
        assert!(reg.check_for(&mt("image/png")).is_none());
        assert_eq!(reg.verify(&mt("image/png"), b"anything"), Ok(()));

        let check = Arc::new(FnCheck::new(
            "png-magic",
            |t: &MimeType, bytes: &[u8]| match bytes.starts_with(b"\x89PNG") {
                true => Ok(()),
                false => Err(format!("not a {t} header")),
            },
        ));
        reg.attach_check("Image/*", check.clone()).unwrap();
        assert!(reg.check_for(&mt("image/png")).is_some());
        assert_eq!(reg.verify(&mt("image/png"), b"\x89PNG\r\n"), Ok(()));
        assert_eq!(
            reg.verify(&mt("image/png"), b"GIF89a"),
            Err("not a image/png header".to_string())
        );
        assert_eq!(
            reg.verify(&mt("image/gif"), b"GIF89a"),
            Ok(()),
            "the lifted subtype is not checked"
        );
        assert_eq!(reg.verify(&mt("audio/wav"), b"RIFF"), Ok(()));
        assert_eq!(
            reg.verify(&mt("application/x-made-up"), b"??"),
            Ok(()),
            "a type with no row of its own walks up to the floor"
        );
        assert_eq!(
            reg.attach_check("png", check).unwrap_err(),
            RegistryError::Key("png".into())
        );

        // A later row naming a new check drops the compiled one under it,
        // and a clone made before the swap keeps what it had.
        let snapshot = reg.clone();
        let renamed: toml::Table =
            toml::from_str("[\"image/*\"]\ncheck = \"checks/image2.rhai\"\n").unwrap();
        let next = reg.layered(&renamed, "file").unwrap();
        assert!(next.check_for(&mt("image/png")).is_none());
        assert_eq!(
            next.info(&mt("image/png")).check.as_deref(),
            Some("checks/image2.rhai")
        );
        assert!(snapshot.check_for(&mt("image/png")).is_some());
        assert!(format!("{next:?}").contains("checks"));
        // A row that does not mention `check` keeps the attached one.
        let unrelated: toml::Table =
            toml::from_str("[\"image/*\"]\nfamily = \"picture\"\n").unwrap();
        let kept = reg.layered(&unrelated, "file").unwrap();
        assert!(kept.check_for(&mt("image/png")).is_some());
        assert!(reg.layered(&bad_key_table(), "file").is_err());
    }

    fn bad_key_table() -> toml::Table {
        toml::from_str("[png]\nfamily = \"image\"").unwrap()
    }

    #[test]
    fn stand_in_defaults() {
        let reg = MimeRegistry::builtin();
        let png = reg.info(&mt("image/png"));
        assert_eq!(
            png.render_stand_in(Some("hero.png"), 240 * 1024, Some((1024, 768)), None, None),
            "[image/png 1024x768, 240 KB] hero.png"
        );
        let wav = reg.info(&mt("audio/wav"));
        assert_eq!(
            wav.render_stand_in(None, 2048, None, Some(2500), None),
            "[audio/wav 2.5s, 2 KB]"
        );
        // A document's stand-in counts its pages, singular included.
        let pdf = reg.info(&MimeType::parse("application/pdf").unwrap());
        assert_eq!(
            pdf.render_stand_in(Some("brief.pdf"), 2048, None, None, Some(1)),
            "[application/pdf 1 page, 2 KB] brief.pdf"
        );
        assert_eq!(
            pdf.render_stand_in(None, 2048, None, None, Some(12)),
            "[application/pdf 12 pages, 2 KB]"
        );
        assert_eq!(
            reg.info(&mt("model/stl"))
                .render_stand_in(Some("a"), 1, None, None, None),
            "[model/stl, 1 B] a"
        );
    }
}
