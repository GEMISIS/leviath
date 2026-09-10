//! The run's blob store, as the built-in tools see it.
//!
//! A tool that reads a file the model cannot take as text, or that receives
//! bytes from an MCP server, has somewhere to put them: the store the run
//! was spawned with. The tool never learns where that is; it hands over a
//! [`Blob`] and gets back the [`Part`] a region entry carries.

use std::sync::Arc;

use leviath_core::mime::{Blob, BlobStore, MimeType, Part, RegistryCell};

/// Where a tool's bytes go, and what types them.
#[derive(Clone)]
pub struct ToolMime {
    /// The run's blob store.
    pub store: Arc<dyn BlobStore>,
    /// The registry that types bytes and prices them: the run's own, read
    /// through the cell the runtime swaps a reloaded registry into.
    pub registry: Arc<RegistryCell>,
    /// The run the bytes belong to.
    pub run_id: String,
    /// The largest part a tool may store, in bytes (`[mime] max_part_bytes`).
    pub max_part_bytes: u64,
}

impl std::fmt::Debug for ToolMime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolMime")
            .field("run_id", &self.run_id)
            .field("max_part_bytes", &self.max_part_bytes)
            .finish()
    }
}

impl ToolMime {
    /// Store `blob` for this run and return the part that refers to it.
    ///
    /// Refuses a blob over the size ceiling by name, so the tool can report
    /// it to the model in its own words.
    pub fn store(&self, blob: Blob) -> Result<Part, String> {
        let name = blob.name.clone().unwrap_or_else(|| "part".to_string());
        if blob.bytes.len() as u64 > self.max_part_bytes {
            return Err(format!(
                "'{name}' is {} bytes, over the {} byte ceiling ([mime] max_part_bytes)",
                blob.bytes.len(),
                self.max_part_bytes
            ));
        }
        let reference = self
            .store
            .put(&self.run_id, &blob, &self.registry.load())
            .map_err(|e| format!("could not store '{name}': {e}"))?;
        Ok(Part::stored(reference).named(name))
    }

    /// Type `bytes` that arrived as `declared` (an MCP `mimeType`, say) under
    /// `name`, letting the registry correct a declaration it cannot parse.
    pub fn type_of(&self, declared: Option<&str>, name: Option<&str>, bytes: &[u8]) -> MimeType {
        let declared = declared.and_then(|d| MimeType::parse(d).ok());
        self.registry.load().resolve(declared.as_ref(), name, bytes)
    }

    /// A file name for bytes that arrived without one: `stem` plus the type's
    /// first extension, or the stem alone when the registry knows none.
    pub fn name_for(&self, stem: &str, mime_type: &MimeType) -> String {
        match self.registry.load().info(mime_type).extensions.first() {
            Some(ext) => format!("{stem}.{ext}"),
            None => stem.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::{MemoryBlobStore, MimeRegistry};

    fn mime(max: u64) -> ToolMime {
        ToolMime {
            store: Arc::new(MemoryBlobStore::new()),
            registry: Arc::new(RegistryCell::default()),
            run_id: "run-1".to_string(),
            max_part_bytes: max,
        }
    }

    #[test]
    fn stores_types_and_names() {
        let m = mime(1024);
        let png = MimeType::parse("image/png").unwrap();
        let part = m
            .store(Blob::new(png.clone(), b"\x89PNG\r\n\x1a\n".to_vec()).named("a.png"))
            .unwrap();
        assert_eq!(part.name.as_deref(), Some("a.png"));
        assert!(m.store.has("run-1", &part.blob().unwrap().sha256));
        let unnamed = m.store(Blob::new(png.clone(), vec![1])).unwrap();
        assert_eq!(unnamed.name.as_deref(), Some("part"));
        let err = mime(2)
            .store(Blob::new(png.clone(), vec![1, 2, 3]))
            .unwrap_err();
        assert!(err.contains("over the 2 byte ceiling"), "{err}");

        assert_eq!(
            m.type_of(Some("image/png"), None, b"zzz").as_str(),
            "image/png"
        );
        assert_eq!(
            m.type_of(Some("not a type"), None, b"\x89PNG\r\n\x1a\n")
                .as_str(),
            "image/png"
        );
        assert_eq!(m.name_for("image-1", &png), "image-1.png");
        struct Broken;
        impl BlobStore for Broken {
            fn put(
                &self,
                _: &str,
                _: &Blob,
                _: &MimeRegistry,
            ) -> std::io::Result<leviath_core::mime::BlobRef> {
                Err(std::io::Error::other("disk is full"))
            }
            fn read(&self, _: &str, _: &str) -> std::io::Result<Arc<[u8]>> {
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
        let broken = ToolMime {
            store: Arc::new(Broken),
            registry: Arc::new(RegistryCell::default()),
            run_id: "run-1".to_string(),
            max_part_bytes: 1024,
        };
        let err = broken
            .store(Blob::new(png.clone(), vec![1]).named("x.png"))
            .unwrap_err();
        assert!(err.contains("could not store 'x.png'"), "{err}");
        let odd = MimeType::parse("application/x-nothing").unwrap();
        assert_eq!(m.name_for("blob-1", &odd), "blob-1");
        assert!(format!("{m:?}").contains("run-1"));
    }
}
