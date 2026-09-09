//! Where a stored part's bytes live.
//!
//! A [`BlobStore`] holds bytes once per run under their SHA-256. Everything
//! else in the engine carries a [`BlobRef`], so the journal, the snapshots and
//! the events never grow by a file's size. The daemon backs this with a
//! directory beside the run; tests and the embedding API use
//! [`MemoryBlobStore`].

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::part::{Blob, BlobRef};
use super::registry::MediaRegistry;

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Whether `s` is exactly a lowercase hex SHA-256, which is the only shape a
/// store key may have. Checked before any key touches a path.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The reference for a key that is not a valid hash.
fn bad_key(sha256: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("'{sha256}' is not a sha256 hex digest"),
    )
}

/// Bytes that fail the check their type puts on them.
///
/// Every store runs this first thing in `put`, so one line covers every
/// ingress there is: the check a row names is asked once, where the bytes
/// come to rest, and never has to be remembered at a call site.
pub fn verify_blob(reg: &MediaRegistry, blob: &Blob) -> io::Result<()> {
    reg.verify(&blob.media_type, &blob.bytes).map_err(|why| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} refused as {}: {why}",
                blob.name.as_deref().unwrap_or("the bytes"),
                blob.media_type
            ),
        )
    })
}

/// Bytes, once per run, by hash.
pub trait BlobStore: Send + Sync {
    /// Store `blob` for `run_id` and return its reference. Storing the same
    /// bytes twice is one file and the same reference. Bytes that fail the
    /// check their type's row names are refused (`InvalidData`) with the
    /// reason; see [`verify_blob`].
    fn put(&self, run_id: &str, blob: &Blob, reg: &MediaRegistry) -> io::Result<BlobRef>;

    /// The bytes stored under `sha256` for `run_id`.
    fn read(&self, run_id: &str, sha256: &str) -> io::Result<Arc<[u8]>>;

    /// Copy one blob from a run to another, so a child or a fan-out worker
    /// can reference it without the parent's directory.
    fn copy(&self, from_run: &str, to_run: &str, sha256: &str) -> io::Result<()>;

    /// Every hash stored for `run_id`, in no particular order.
    fn list(&self, run_id: &str) -> io::Result<Vec<String>>;

    /// Whether `sha256` is stored for `run_id`.
    fn has(&self, run_id: &str, sha256: &str) -> bool {
        self.read(run_id, sha256).is_ok()
    }
}

/// One run's blobs, by hash.
type RunBlobs = HashMap<String, Arc<[u8]>>;

/// A store that keeps everything in memory.
#[derive(Debug, Default)]
pub struct MemoryBlobStore {
    runs: Mutex<HashMap<String, RunBlobs>>,
}

impl MemoryBlobStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many bytes the store holds for `run_id`.
    pub fn bytes_for(&self, run_id: &str) -> u64 {
        crate::sync::lock(&self.runs)
            .get(run_id)
            .map(|m| m.values().map(|v| v.len() as u64).sum())
            .unwrap_or(0)
    }
}

impl BlobStore for MemoryBlobStore {
    fn put(&self, run_id: &str, blob: &Blob, reg: &MediaRegistry) -> io::Result<BlobRef> {
        verify_blob(reg, blob)?;
        let r = blob.describe(reg);
        crate::sync::lock(&self.runs)
            .entry(run_id.to_string())
            .or_default()
            .entry(r.sha256.clone())
            .or_insert_with(|| Arc::from(blob.bytes.as_slice()));
        Ok(r)
    }

    fn read(&self, run_id: &str, sha256: &str) -> io::Result<Arc<[u8]>> {
        if !is_sha256_hex(sha256) {
            return Err(bad_key(sha256));
        }
        crate::sync::lock(&self.runs)
            .get(run_id)
            .and_then(|m| m.get(sha256))
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no blob {sha256}")))
    }

    fn copy(&self, from_run: &str, to_run: &str, sha256: &str) -> io::Result<()> {
        let bytes = self.read(from_run, sha256)?;
        crate::sync::lock(&self.runs)
            .entry(to_run.to_string())
            .or_default()
            .insert(sha256.to_string(), bytes);
        Ok(())
    }

    fn list(&self, run_id: &str) -> io::Result<Vec<String>> {
        Ok(self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaType;

    #[test]
    fn hashes_are_lowercase_hex() {
        let h = sha256_hex(b"abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_sha256_hex(&h));
        assert!(!is_sha256_hex(&h.to_uppercase()));
        assert!(!is_sha256_hex("abc"));
        assert!(!is_sha256_hex(&"g".repeat(64)));
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryBlobStore::new();
        let reg = MediaRegistry::builtin();
        let blob = Blob::new(MediaType::parse("image/png").unwrap(), vec![1, 2, 3]);
        let r1 = store.put("run-a", &blob, &reg).unwrap();
        let r2 = store.put("run-a", &blob, &reg).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(store.bytes_for("run-a"), 3);
        assert_eq!(store.bytes_for("run-b"), 0);
        assert_eq!(&*store.read("run-a", &r1.sha256).unwrap(), &[1, 2, 3]);
        assert!(store.has("run-a", &r1.sha256));
        assert!(!store.has("run-b", &r1.sha256));
        assert_eq!(store.list("run-a").unwrap(), vec![r1.sha256.clone()]);
        assert!(store.list("run-b").unwrap().is_empty());
        let missing = store.read("run-a", &"0".repeat(64)).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        let bad = store.read("run-a", "../etc/passwd").unwrap_err();
        assert_eq!(bad.kind(), io::ErrorKind::InvalidInput);
        assert!(bad.to_string().contains("sha256"));
        store.copy("run-a", "run-b", &r1.sha256).unwrap();
        assert!(store.has("run-b", &r1.sha256));
        assert!(store.copy("run-a", "run-c", &"0".repeat(64)).is_err());
        assert!(format!("{store:?}").contains("MemoryBlobStore"));
    }

    /// The check a row names runs where the bytes come to rest, so a store
    /// refuses bytes that fail it with the reason and the name.
    #[test]
    fn a_store_refuses_bytes_that_fail_their_types_check() {
        use crate::media::FnCheck;
        let store = MemoryBlobStore::new();
        let mut reg = MediaRegistry::builtin();
        let table: toml::Table = toml::from_str("[\"image/png\"]\ncheck = \"png.rhai\"\n").unwrap();
        reg.layer(&table, "t").unwrap();
        reg.attach_check(
            "image/png",
            Arc::new(FnCheck::new(
                "png",
                |_: &MediaType, bytes: &[u8]| match bytes.starts_with(b"\x89PNG") {
                    true => Ok(()),
                    false => Err("no PNG signature".to_string()),
                },
            )),
        )
        .unwrap();
        let png = MediaType::parse("image/png").unwrap();
        let fake = Blob::new(png.clone(), b"GIF89a".to_vec()).named("shot.png");
        let err = store.put("run-a", &fake, &reg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            "shot.png refused as image/png: no PNG signature"
        );
        assert!(
            store.list("run-a").unwrap().is_empty(),
            "nothing was stored"
        );
        let unnamed = Blob::new(png.clone(), b"GIF89a".to_vec());
        assert!(
            store
                .put("run-a", &unnamed, &reg)
                .unwrap_err()
                .to_string()
                .starts_with("the bytes refused as")
        );
        let real = Blob::new(png, b"\x89PNG\r\n\x1a\n".to_vec());
        assert!(store.put("run-a", &real, &reg).is_ok());
    }

    #[test]
    fn a_poisoned_lock_still_answers() {
        let store = Arc::new(MemoryBlobStore::new());
        let s2 = Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _guard = s2.runs.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(store.bytes_for("x"), 0);
        assert!(store.list("x").unwrap().is_empty());
    }
}
