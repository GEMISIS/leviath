//! What a region entry holds: a list of typed parts, and the text they read as.
//!
//! An entry used to be a `String`. It is now any number of [`Part`]s, each
//! with a media type: a paragraph, an image, a clip. Text parts keep their
//! bytes inline; every other part is a reference into the run's blob store.
//! The text those parts *read as* is kept beside them, rendered once when the
//! content is built: inline text as it is, and for each stored part the
//! stand-in its registry row produced at ingest. Everything that only knows
//! how to read text (the journal digest, a log line, a search index, an older
//! provider) reads that, through `Deref<Target = str>`, and never has to
//! learn what a part is.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::media::{Part, PartBody};

/// The typed content of one entry.
#[derive(Clone, PartialEq, Eq)]
pub struct EntryContent {
    parts: Vec<Part>,
    text: String,
}

impl EntryContent {
    /// One `text/plain` part.
    pub fn text(s: impl Into<String>) -> Self {
        Self::from_parts(vec![Part::text(s)])
    }

    /// Any parts, in order. The text rendering is built here, once.
    pub fn from_parts(parts: Vec<Part>) -> Self {
        let text = render(&parts);
        Self { parts, text }
    }

    /// The parts, in order.
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// The parts, giving up the rendering.
    pub fn into_parts(self) -> Vec<Part> {
        self.parts
    }

    /// The text this content reads as.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The text this content reads as, owned.
    pub fn into_string(self) -> String {
        self.text
    }

    /// Whether every part is inline text.
    pub fn is_text_only(&self) -> bool {
        self.parts.iter().all(|p| !p.is_stored())
    }

    /// Whether any part is stored by reference.
    pub fn has_stored(&self) -> bool {
        !self.is_text_only()
    }

    /// The stored parts, in order.
    pub fn stored(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().filter(|p| p.is_stored())
    }

    /// How many parts are stored by reference.
    pub fn stored_count(&self) -> usize {
        self.stored().count()
    }

    /// The inline text alone, without the stand-ins the stored parts render
    /// as: what a caller that will rebuild the content around the same
    /// stored parts starts from.
    pub fn inline_text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let PartBody::Inline(s) = &p.body {
                if !out.is_empty() && !out.ends_with('\n') && !s.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
        out
    }

    /// The tokens this content is expected to cost, without a registry: the
    /// text heuristic for inline text and the estimate each stored part
    /// carried out of the store.
    pub fn tokens_hint(&self) -> usize {
        self.parts
            .iter()
            .map(|p| match &p.body {
                PartBody::Inline(s) => crate::text::estimate_tokens(s),
                PartBody::Stored(b) => b.tokens,
            })
            .sum()
    }

    /// The same content with `part` appended.
    pub fn with_part(mut self, part: Part) -> Self {
        self.parts.push(part);
        self.text = render(&self.parts);
        self
    }

    /// Whether this is exactly one unnamed `text/plain` part with no delivery
    /// override, which is what a bare string serialises as.
    fn is_plain_string(&self) -> bool {
        match self.parts.as_slice() {
            [p] => {
                p.name.is_none()
                    && p.deliver.is_none()
                    && p.media_type.as_str() == "text/plain"
                    && matches!(p.body, PartBody::Inline(_))
            }
            _ => false,
        }
    }
}

/// Inline text as it is, a stored part as its stand-in, one per line.
fn render(parts: &[Part]) -> String {
    let mut out = String::new();
    for p in parts {
        let s = p.stand_in();
        if !out.is_empty() && !out.ends_with('\n') && !s.is_empty() {
            out.push('\n');
        }
        out.push_str(&s);
    }
    out
}

impl Default for EntryContent {
    fn default() -> Self {
        Self::text("")
    }
}

impl Deref for EntryContent {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl AsRef<str> for EntryContent {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

impl std::borrow::Borrow<str> for EntryContent {
    fn borrow(&self) -> &str {
        &self.text
    }
}

impl FromIterator<EntryContent> for String {
    fn from_iter<I: IntoIterator<Item = EntryContent>>(iter: I) -> Self {
        iter.into_iter().map(|c| c.text).collect()
    }
}

impl<'a> FromIterator<&'a EntryContent> for String {
    fn from_iter<I: IntoIterator<Item = &'a EntryContent>>(iter: I) -> Self {
        iter.into_iter().map(|c| c.text.as_str()).collect()
    }
}

impl fmt::Display for EntryContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Debug for EntryContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_plain_string() {
            fmt::Debug::fmt(&self.text, f)
        } else {
            f.debug_struct("EntryContent")
                .field("parts", &self.parts)
                .field("text", &self.text)
                .finish()
        }
    }
}

impl Hash for EntryContent {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
        for p in &self.parts {
            p.media_type.as_str().hash(state);
            p.name.hash(state);
            if let Some(b) = p.blob() {
                b.sha256.hash(state);
            }
        }
    }
}

impl From<String> for EntryContent {
    fn from(s: String) -> Self {
        Self::text(s)
    }
}

impl From<&str> for EntryContent {
    fn from(s: &str) -> Self {
        Self::text(s)
    }
}

impl From<Vec<Part>> for EntryContent {
    fn from(parts: Vec<Part>) -> Self {
        Self::from_parts(parts)
    }
}

impl From<EntryContent> for String {
    fn from(c: EntryContent) -> Self {
        c.text
    }
}

impl PartialEq<str> for EntryContent {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for EntryContent {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for EntryContent {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl PartialEq<EntryContent> for str {
    fn eq(&self, other: &EntryContent) -> bool {
        self == other.text
    }
}

impl PartialEq<EntryContent> for &str {
    fn eq(&self, other: &EntryContent) -> bool {
        *self == other.text
    }
}

impl PartialEq<EntryContent> for String {
    fn eq(&self, other: &EntryContent) -> bool {
        *self == other.text
    }
}

/// The wire shape: a bare string for plain text, so a snapshot written
/// before parts existed reads back unchanged, and a list of parts otherwise.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Wire {
    Text(String),
    Parts(Vec<Part>),
}

impl Serialize for EntryContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.is_plain_string() {
            serializer.serialize_str(&self.text)
        } else {
            self.parts.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for EntryContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Text(s) => Self::text(s),
            Wire::Parts(parts) => Self::from_parts(parts),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Blob, MediaRegistry, MediaType};

    fn stored(name: &str) -> Part {
        let reg = MediaRegistry::builtin();
        let blob = Blob::new(MediaType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
        Part::stored(blob.describe(&reg)).named(name)
    }

    #[test]
    fn text_reads_as_itself_and_serialises_as_a_string() {
        let c = EntryContent::text("hello");
        assert_eq!(c.as_str(), "hello");
        assert_eq!(&*c, "hello");
        assert_eq!(c.as_ref(), "hello");
        assert_eq!(c.to_string(), "hello");
        assert_eq!(format!("{c:?}"), "\"hello\"");
        assert!(c.is_text_only());
        assert!(!c.has_stored());
        assert_eq!(c.stored_count(), 0);
        assert_eq!(c.parts().len(), 1);
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"hello\"");
        let back: EntryContent = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(back, c);
        assert_eq!(EntryContent::default().as_str(), "");
        assert_eq!(c.clone().into_string(), "hello");
        let s: String = c.clone().into();
        assert_eq!(s, "hello");
        assert_eq!(c.clone().into_parts().len(), 1);
    }

    #[test]
    fn equality_with_strings_in_both_directions() {
        let c = EntryContent::from("x");
        let owned = String::from("x");
        assert!(c == "x");
        assert!(c == *"x");
        assert!(c == owned);
        assert!("x" == c);
        assert!(*"x" == c);
        assert!(owned == c);
        let from_string: EntryContent = String::from("y").into();
        assert_ne!(from_string, c);
        let list = vec![EntryContent::text("a"), EntryContent::text("b")];
        assert_eq!(list.join("\n"), "a\nb");
        let joined: String = list.iter().collect();
        assert_eq!(joined, "ab");
        let owned: String = list.into_iter().collect();
        assert_eq!(owned, "ab");
    }

    #[test]
    fn stored_parts_render_their_stand_in_and_serialise_as_parts() {
        let c = EntryContent::from_parts(vec![Part::text("look:"), stored("a.png")]);
        assert_eq!(c.as_str(), "look:\n[image/png, 3 B] a.png");
        assert!(c.has_stored());
        assert_eq!(c.stored_count(), 1);
        assert!(format!("{c:?}").contains("EntryContent"));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.starts_with('['), "{json}");
        assert!(json.contains("\"sha256\""));
        let back: EntryContent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
        let with = EntryContent::text("first\n").with_part(stored("b.png"));
        assert_eq!(with.as_str(), "first\n[image/png, 3 B] b.png");
        let parts_only: EntryContent = vec![stored("c.png")].into();
        assert_eq!(parts_only.parts().len(), 1);
    }

    #[test]
    fn a_named_or_typed_text_part_is_not_a_bare_string() {
        let named = EntryContent::from_parts(vec![Part::text("t").named("n")]);
        assert!(serde_json::to_string(&named).unwrap().starts_with('['));
        let md = EntryContent::from_parts(vec![Part::inline(
            MediaType::parse("text/markdown").unwrap(),
            "# t",
        )]);
        assert!(serde_json::to_string(&md).unwrap().starts_with('['));
        let two = EntryContent::from_parts(vec![Part::text("a"), Part::text("b")]);
        assert_eq!(two.as_str(), "a\nb");
        assert!(serde_json::to_string(&two).unwrap().starts_with('['));
        let reg = MediaRegistry::builtin();
        let big_text = Blob::new(
            MediaType::parse("text/plain").unwrap(),
            b"stored text".to_vec(),
        );
        let stored_text = EntryContent::from_parts(vec![Part::stored(big_text.describe(&reg))]);
        assert!(
            serde_json::to_string(&stored_text)
                .unwrap()
                .starts_with('[')
        );
        assert!(serde_json::from_str::<EntryContent>("42").is_err());
        assert!(serde_json::from_str::<EntryContent>("{\"x\":1}").is_err());
        let empty = EntryContent::from_parts(vec![]);
        assert_eq!(empty.as_str(), "");
        assert!(serde_json::to_string(&empty).unwrap().starts_with('['));
    }

    #[test]
    fn a_region_with_accepts_refuses_other_types_and_says_what_it_takes() {
        use crate::region::{Region, RegionKind};
        let mut region = Region::new("art".into(), RegionKind::Pinned, 10_000);
        region.accepts = vec!["image/*".into(), "text/plain".into()];
        region
            .add_entry(
                EntryContent::from_parts(vec![Part::text("caption"), stored("a.png")]),
                5,
            )
            .unwrap();
        let err = region
            .add_entry(
                EntryContent::from_parts(vec![Part::inline(
                    MediaType::parse("text/markdown").unwrap(),
                    "# no",
                )]),
                5,
            )
            .unwrap_err();
        assert!(err.to_string().contains("image/*, text/plain"), "{err}");
        assert!(err.to_string().contains("text/markdown"), "{err}");
        assert!(region.accepts_content(&EntryContent::text("ok")).is_ok());
        let open = Region::new("any".into(), RegionKind::Pinned, 10);
        assert!(open.accepts_content(&EntryContent::text("x")).is_ok());
        assert_eq!(region.stored_count(), 1);
    }

    #[test]
    fn max_stored_evicts_the_oldest_stored_entry_or_refuses() {
        use crate::region::{Admission, Region, RegionKind};
        let mut region = Region::new("sprites".into(), RegionKind::Temporary, 10_000);
        region.max_stored = Some(2);
        region
            .add_entry(EntryContent::text("just text"), 1)
            .unwrap();
        region
            .add_entry(EntryContent::from_parts(vec![stored("a.png")]), 5)
            .unwrap();
        region
            .add_entry(EntryContent::from_parts(vec![stored("b.png")]), 5)
            .unwrap();
        assert_eq!(region.stored_count(), 2);
        region
            .add_entry(EntryContent::from_parts(vec![stored("c.png")]), 5)
            .unwrap();
        assert_eq!(region.stored_count(), 2);
        assert_eq!(region.content.len(), 3, "the text entry stays; a.png went");
        assert_eq!(region.current_tokens, 11);
        let names: Vec<_> = region
            .content
            .iter()
            .flat_map(|e| e.content.parts().iter().filter_map(|p| p.name.clone()))
            .collect();
        assert_eq!(names, vec!["b.png", "c.png"]);
        let too_many = region
            .add_entry(
                EntryContent::from_parts(vec![stored("d.png"), stored("e.png"), stored("f.png")]),
                5,
            )
            .unwrap_err();
        assert!(too_many.to_string().contains("at most 2"), "{too_many}");
        region.admission = Admission::Reject;
        let refused = region
            .add_entry(EntryContent::from_parts(vec![stored("g.png")]), 5)
            .unwrap_err();
        assert!(
            refused.to_string().contains("release one first"),
            "{refused}"
        );
        assert_eq!(region.stored_count(), 2);
        region
            .add_entry(EntryContent::text("text still lands"), 1)
            .unwrap();
        // Asked to shed more than it holds, the eviction stops at the last
        // stored entry rather than looping.
        region.evict_oldest_stored(10);
        assert_eq!(region.stored_count(), 0);
        assert_eq!(region.content.len(), 2, "text entries stay");
    }

    #[test]
    fn a_region_with_a_schema_takes_text_only() {
        use crate::region::schema::{ContentFormat, RegionSchema};
        use crate::region::{Region, RegionKind};
        let mut region = Region::new("plan".into(), RegionKind::Pinned, 10_000);
        region.schema = Some(RegionSchema::new(ContentFormat::Json));
        region.add_entry(EntryContent::text("{}"), 1).unwrap();
        let err = region
            .add_entry(EntryContent::from_parts(vec![stored("a.png")]), 5)
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot hold a stored part"),
            "{err}"
        );
    }

    #[test]
    fn inline_text_and_token_hint_leave_the_stand_ins_out() {
        let c = EntryContent::from_parts(vec![
            Part::text("one"),
            stored("a.png"),
            Part::text(""),
            Part::text("two\n"),
            Part::text("three"),
        ]);
        assert_eq!(c.inline_text(), "one\ntwo\nthree");
        assert_eq!(
            c.tokens_hint(),
            crate::text::estimate_tokens("one")
                + stored("a.png").blob().unwrap().tokens
                + crate::text::estimate_tokens("two\n")
                + crate::text::estimate_tokens("three")
        );
        assert_eq!(EntryContent::text("").inline_text(), "");
    }

    #[test]
    fn hashing_sees_parts_not_only_text() {
        use std::collections::hash_map::DefaultHasher;
        let h = |c: &EntryContent| {
            let mut s = DefaultHasher::new();
            c.hash(&mut s);
            s.finish()
        };
        let a = EntryContent::from_parts(vec![Part::text("x"), stored("a.png")]);
        let b = EntryContent::from_parts(vec![Part::text("x"), stored("a.png").named("a.png")]);
        assert_eq!(h(&a), h(&b));
        let c = EntryContent::text("x\n[image/png, 3 B] a.png");
        assert_ne!(h(&a), h(&c), "same text, different parts");
    }
}
