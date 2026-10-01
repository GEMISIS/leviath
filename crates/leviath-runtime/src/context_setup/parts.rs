//! Files sent to a run becoming stored parts.
//!
//! An [`InboundPart`] carries bytes; a region entry carries a reference. The
//! crossing happens here: the bytes are typed by the registry, written to the
//! run's blob store, and the reference is what goes into the region, beside
//! whatever caption came with it.

use leviath_core::mime::{Blob, BlobStore, InboundPart, MimeRegistry, Part};
use leviath_core::region::EntryContent;

/// Where ingested bytes go and what types them.
pub(crate) struct PartSink<'a> {
    /// The run's blob store.
    pub store: &'a dyn BlobStore,
    /// The registry that types the bytes.
    pub registry: &'a MimeRegistry,
    /// The run the bytes belong to.
    pub run_id: &'a str,
    /// The largest part accepted, in bytes.
    pub max_part_bytes: u64,
    /// The most text an entry carries inline; past it the text is stored.
    pub inline_text_bytes: u64,
}

impl<'a> PartSink<'a> {
    /// The sink for `run_id` over what the world holds: `None` when it has no
    /// store or no registry, in which case nothing typed can be stored.
    pub(crate) fn over(
        sources: &'a crate::blob_store::HydrationSources,
        run_id: &'a str,
        mime: &crate::blob_store::MimeParams<'_, '_>,
    ) -> Option<Self> {
        sources.as_ref().map(|(store, registry)| PartSink {
            store: store.as_ref(),
            registry,
            run_id,
            max_part_bytes: mime.max_part_bytes(),
            inline_text_bytes: mime.inline_text_bytes(),
        })
    }

    /// `text` as the part an entry carries: inline while it is within
    /// `[mime] inline_text_bytes`, otherwise stored as `text/plain` under
    /// `name` so the region holds a reference and a stand-in rather than the
    /// whole transcript, and the bytes are reachable by hash like any other
    /// part's.
    pub(crate) fn admit_text(&self, name: &str, text: &str) -> Result<Part, String> {
        if text.len() as u64 <= self.inline_text_bytes {
            return Ok(Part::text(text));
        }
        let inbound = InboundPart::from_bytes(name, text.as_bytes().to_vec())
            .typed(leviath_core::mime::text_plain());
        self.store_part(&inbound)
    }

    /// Type and store one inbound part, returning the region part for it.
    pub(crate) fn store_part(&self, inbound: &InboundPart) -> Result<Part, String> {
        if inbound.data.len() as u64 > self.max_part_bytes {
            return Err(format!(
                "part '{}' is {} bytes, over the {} byte ceiling ([mime] max_part_bytes)",
                inbound.name,
                inbound.data.len(),
                self.max_part_bytes
            ));
        }
        let mime_type = self.registry.resolve(
            inbound.mime_type.as_ref(),
            Some(&inbound.name),
            &inbound.data,
        );
        let blob = Blob::new(mime_type, inbound.data.clone()).named(&inbound.name);
        let reference = self
            .store
            .put(self.run_id, &blob, self.registry)
            .map_err(|e| format!("could not store part '{}': {e}", inbound.name))?;
        let mut part = Part::stored(reference).named(&inbound.name);
        part.deliver = inbound.deliver;
        Ok(part)
    }

    /// The entry an inbound part writes: its caption, when it has one, then
    /// the stored part.
    pub(crate) fn entry_for(&self, inbound: &InboundPart) -> Result<EntryContent, String> {
        let stored = self.store_part(inbound)?;
        let mut parts = Vec::new();
        if let Some(caption) = inbound.caption.as_deref().filter(|c| !c.trim().is_empty()) {
            parts.push(Part::text(caption));
        }
        parts.push(stored);
        Ok(EntryContent::from_parts(parts))
    }

    /// The tokens an entry costs its region, charged by this sink's registry.
    pub(crate) fn tokens_for(&self, content: &EntryContent) -> usize {
        content.tokens(Some(self.registry))
    }
}

/// `text` as a part: through [`PartSink::admit_text`] when a sink is at hand,
/// inline otherwise. A store that refuses the text keeps it inline and says
/// so, since a reply or a result the model must see is not something to
/// lose over a full store.
pub(crate) fn text_part(sink: Option<&PartSink<'_>>, name: &str, text: &str) -> Part {
    let Some(sink) = sink else {
        return Part::text(text);
    };
    sink.admit_text(name, text).unwrap_or_else(|e| {
        tracing::warn!(part = name, error = %e, "text stays inline: the store refused it");
        Part::text(text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::MemoryBlobStore;

    /// Text within the inline ceiling stays inline; past it, it is stored as
    /// `text/plain` under its name, and past the part ceiling the store
    /// refuses it like any other part.
    #[test]
    fn text_is_stored_past_the_inline_ceiling() {
        let store = MemoryBlobStore::new();
        let registry = MimeRegistry::builtin();
        let sink = PartSink {
            store: &store,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 64,
            inline_text_bytes: 8,
        };
        assert!(!sink.admit_text("r.txt", "short").unwrap().is_stored());
        let stored = sink.admit_text("r.txt", "well past eight bytes").unwrap();
        assert!(stored.is_stored());
        assert_eq!(stored.mime_type.as_str(), "text/plain");
        assert_eq!(stored.name.as_deref(), Some("r.txt"));
        let err = sink.admit_text("r.txt", &"z".repeat(65)).unwrap_err();
        assert!(err.contains("over the 64 byte ceiling"), "{err}");
    }
}
