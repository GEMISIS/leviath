//! Attached files becoming stored parts in the regions they were sent to.
//!
//! An [`InboundPart`] carries bytes; a region entry carries a reference. The
//! crossing happens here, once per run: the bytes are typed by the registry,
//! written to the run's blob store, and the reference is written into the
//! region beside whatever caption came with it. A region that refuses the
//! type (`accepts`) or is full (`max_stored` under `admission = "reject"`)
//! refuses the spawn or the message, naming the part, rather than dropping
//! it where nobody would notice.

use leviath_core::media::{Blob, BlobStore, InboundPart, MediaRegistry, Part};
use leviath_core::region::EntryContent;

use crate::components::ContextWindow;

/// Where ingested bytes go and what types them.
pub(crate) struct PartSink<'a> {
    /// The run's blob store.
    pub store: &'a dyn BlobStore,
    /// The registry that types the bytes.
    pub registry: &'a MediaRegistry,
    /// The run the bytes belong to.
    pub run_id: &'a str,
    /// The largest part accepted, in bytes.
    pub max_part_bytes: u64,
}

impl PartSink<'_> {
    /// Type and store one inbound part, returning the region part for it.
    pub(crate) fn store_part(&self, inbound: &InboundPart) -> Result<Part, String> {
        if inbound.data.len() as u64 > self.max_part_bytes {
            return Err(format!(
                "part '{}' is {} bytes, over the {} byte ceiling ([media] max_part_bytes)",
                inbound.name,
                inbound.data.len(),
                self.max_part_bytes
            ));
        }
        let media_type = self.registry.resolve(
            inbound.media_type.as_ref(),
            Some(&inbound.name),
            &inbound.data,
        );
        let blob = Blob::new(media_type, inbound.data.clone()).named(&inbound.name);
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

    /// The tokens an entry costs its region: text by the byte heuristic, a
    /// stored part by its registry estimate.
    pub(crate) fn tokens_for(&self, content: &EntryContent) -> usize {
        content
            .parts()
            .iter()
            .map(|p| p.tokens(self.registry))
            .sum()
    }
}

/// Write every attached part into its region, after the seeds are in.
///
/// A part with no region goes to the task region, which is where the text it
/// came with went. A part naming a region the blueprint does not declare is
/// refused: the CLI checks this before dialling, and the API and ACP paths
/// reach here directly.
pub(crate) fn ingest_parts(
    window: &mut ContextWindow,
    blueprint: &leviath_core::Blueprint,
    parts: Vec<InboundPart>,
    sink: &PartSink<'_>,
) -> Result<(), String> {
    for inbound in parts {
        let region = match &inbound.region {
            Some(name) => name.clone(),
            None => super::task_region_name(blueprint).ok_or_else(|| {
                format!(
                    "part '{}' names no region and this agent takes no task",
                    inbound.name
                )
            })?,
        };
        let Some(target) = window.get_region(&region) else {
            return Err(format!(
                "part '{}' names region '{region}', which this agent does not declare",
                inbound.name
            ));
        };
        // A caption is text, and a region that takes only images has no
        // room for it. Refusing beats quietly dropping what the user wrote.
        let captioned = inbound
            .caption
            .as_deref()
            .is_some_and(|c| !c.trim().is_empty());
        if captioned
            && !target.accepts.is_empty()
            && !leviath_core::media::text_plain().matches_any(&target.accepts)
        {
            return Err(format!(
                "the caption on '{}' has nowhere to go: region '{region}' takes {} only; \
                 add text/* to its accepts or send the caption as a message",
                inbound.name,
                target.accepts.join(", ")
            ));
        }
        let content = sink.entry_for(&inbound)?;
        let tokens = sink.tokens_for(&content);
        window
            .add_content_entry(&region, leviath_core::EntryKind::Text, content, tokens)
            .map_err(|e| {
                format!(
                    "part '{}' was refused by region '{region}': {e}",
                    inbound.name
                )
            })?;
        tracing::info!(
            region = %region,
            name = %inbound.name,
            bytes = inbound.data.len(),
            "[media] stored an attached part"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::media::{MediaType, MemoryBlobStore};
    use leviath_core::region::{Region, RegionKind};

    fn window() -> ContextWindow {
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new("task".into(), RegionKind::Pinned, 10_000));
        let mut art = Region::new("art".into(), RegionKind::Pinned, 10_000);
        art.accepts = vec!["image/*".into()];
        window.add_region(art);
        window
    }

    fn blueprint() -> leviath_core::Blueprint {
        leviath_core::manifest::parse_manifest(
            "[agent]\nname = \"t\"\n\n[context.regions]\ntask = { kind = \"pinned\" }\nart = { kind = \"pinned\", accepts = [\"image/*\"] }\n",
        )
        .unwrap()
    }

    #[test]
    fn parts_land_in_their_regions_with_captions_and_types() {
        let store = MemoryBlobStore::new();
        let registry = MediaRegistry::builtin();
        let sink = PartSink {
            store: &store,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 1024,
        };
        let mut window = window();
        let png =
            InboundPart::from_bytes("hero.png", b"\x89PNG\r\n\x1a\nbody".to_vec()).in_region("art");
        let note = InboundPart::from_bytes("notes.txt", b"plain notes".to_vec())
            .typed(MediaType::parse("text/plain").unwrap())
            .delivered(leviath_core::media::Delivery::Text)
            .captioned("my notes");
        ingest_parts(&mut window, &blueprint(), vec![png, note], &sink).unwrap();
        let art = window.get_region("art").unwrap();
        assert_eq!(art.content.len(), 1);
        assert_eq!(
            art.content[0].content.as_str(),
            "[image/png, 12 B] hero.png"
        );
        assert_eq!(art.stored_count(), 1);
        assert!(art.content[0].tokens >= 1600);
        let task = window.get_region("task").unwrap();
        assert_eq!(
            task.content[0].content.as_str(),
            "my notes\n[text/plain, 11 B] notes.txt"
        );
        let part = task.content[0].content.parts()[1].clone();
        assert_eq!(part.media_type.as_str(), "text/plain");
        assert_eq!(part.deliver, Some(leviath_core::media::Delivery::Text));
        assert!(store.has("run-1", &part.blob().unwrap().sha256));
    }

    #[test]
    fn refusals_name_the_part() {
        let store = MemoryBlobStore::new();
        let registry = MediaRegistry::builtin();
        let sink = PartSink {
            store: &store,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 4,
        };
        let mut window = window();
        let big = InboundPart::from_bytes("big.bin", vec![0; 5]);
        let err = ingest_parts(&mut window, &blueprint(), vec![big], &sink).unwrap_err();
        assert!(err.contains("big.bin") && err.contains("ceiling"), "{err}");
        let sink = PartSink {
            store: &store,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 1024,
        };
        let wrong_type = InboundPart::from_bytes("song.wav", vec![1, 2, 3]).in_region("art");
        let err = ingest_parts(&mut window, &blueprint(), vec![wrong_type], &sink).unwrap_err();
        assert!(err.contains("refused by region 'art'"), "{err}");
        let no_region = InboundPart::from_bytes("x.png", vec![1]).in_region("ghost");
        let err = ingest_parts(&mut window, &blueprint(), vec![no_region], &sink).unwrap_err();
        assert!(err.contains("does not declare"), "{err}");
        let captioned = InboundPart::from_bytes("x.png", b"\x89PNG\r\n\x1a\n".to_vec())
            .in_region("art")
            .captioned("look");
        let err = ingest_parts(&mut window, &blueprint(), vec![captioned], &sink).unwrap_err();
        assert!(
            err.contains("caption on 'x.png' has nowhere to go"),
            "{err}"
        );
        let taskless = leviath_core::manifest::parse_manifest(
            "[agent]\nname = \"t\"\n\n[context.regions]\nlog = { kind = \"temporary\" }\n",
        )
        .unwrap();
        let mut window = ContextWindow::new(1000);
        window.add_region(Region::new("log".into(), RegionKind::Temporary, 1000));
        let err = ingest_parts(
            &mut window,
            &taskless,
            vec![InboundPart::from_bytes("x.png", vec![1])],
            &sink,
        )
        .unwrap_err();
        assert!(err.contains("takes no task"), "{err}");
        let err = ingest_parts(
            &mut window,
            &taskless,
            vec![InboundPart::from_bytes("x.png", b"\x89PNG\r\n\x1a\n".to_vec()).in_region("log")],
            &sink,
        )
        .unwrap_err();
        assert!(err.contains("token budget"), "{err}");
        // A store that cannot write reports it by name.
        struct Broken;
        impl BlobStore for Broken {
            fn put(
                &self,
                _: &str,
                _: &Blob,
                _: &MediaRegistry,
            ) -> std::io::Result<leviath_core::media::BlobRef> {
                Err(std::io::Error::other("disk is full"))
            }
            fn read(&self, _: &str, _: &str) -> std::io::Result<std::sync::Arc<[u8]>> {
                Err(std::io::Error::other("no"))
            }
            fn copy(&self, _: &str, _: &str, _: &str) -> std::io::Result<()> {
                Err(std::io::Error::other("no"))
            }
            fn list(&self, _: &str) -> std::io::Result<Vec<String>> {
                Ok(Vec::new())
            }
        }
        assert!(Broken.read("r", "s").is_err());
        assert!(Broken.copy("r", "s", "t").is_err());
        assert!(Broken.list("r").unwrap().is_empty());
        let broken = PartSink {
            store: &Broken,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 1024,
        };
        let err = broken
            .entry_for(&InboundPart::from_bytes("x.png", vec![1]))
            .unwrap_err();
        assert!(err.contains("could not store part 'x.png'"), "{err}");
    }
}
