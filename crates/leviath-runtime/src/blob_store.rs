//! Where a run's stored media parts live on disk, and the world resources
//! that hand the store and the media registry to every system.
//!
//! A part whose bytes are not text is written once under
//! `<runs_dir>/<run_id>/blobs/<sha256>` and referenced by hash everywhere
//! else. Deleting the run deletes its blobs; nothing outside the run's
//! directory points at them. A world with no runs directory (the embedding
//! mode) keeps the same bytes in memory instead.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy_ecs::prelude::Resource;
use leviath_core::files::BLOBS_DIR;
use leviath_core::media::{
    Blob, BlobRef, BlobStore, MediaRegistry, MemoryBlobStore, is_sha256_hex,
};

/// The store every system reads and writes stored parts through.
#[derive(Resource, Clone)]
pub struct BlobStoreHandle(pub Arc<dyn BlobStore>);

/// The media registry a world was built with: the compiled defaults plus the
/// operator's `[media_types]`. A blueprint's own rows layer on top per agent.
#[derive(Resource, Clone)]
pub struct MediaRegistryHandle(pub Arc<MediaRegistry>);

impl Default for MediaRegistryHandle {
    fn default() -> Self {
        Self(Arc::new(MediaRegistry::builtin()))
    }
}

/// The operator's ceilings on typed parts, from `[media]` in the config.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaLimits {
    /// Bytes one part may be; larger is refused wherever it arrives.
    pub max_part_bytes: u64,
    /// Bytes of text kept inline in an entry before the part is stored.
    pub inline_text_bytes: u64,
    /// Stored parts one model request carries before the oldest are dropped.
    pub max_stored_per_request: usize,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_part_bytes: 32 * 1024 * 1024,
            inline_text_bytes: 1024 * 1024,
            max_stored_per_request: 100,
        }
    }
}

/// The store for a world: on disk under `runs_dir`, or in memory without one.
pub fn store_for(runs_dir: Option<&Path>) -> Arc<dyn BlobStore> {
    match runs_dir {
        Some(dir) => Arc::new(FsBlobStore::new(dir.to_path_buf())),
        None => Arc::new(MemoryBlobStore::new()),
    }
}

/// Blobs as files beside the run they belong to.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    runs_dir: PathBuf,
}

/// A run id that could name a path outside its own directory.
fn bad_run_id(run_id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("'{run_id}' is not a run id"),
    )
}

/// A hash that is not a lowercase hex SHA-256, refused before it touches a path.
fn bad_sha(sha256: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("'{sha256}' is not a sha256 hex digest"),
    )
}

impl FsBlobStore {
    /// A store rooted at `runs_dir`, the directory that holds one directory
    /// per run.
    pub fn new(runs_dir: PathBuf) -> Self {
        Self { runs_dir }
    }

    /// The blobs directory for `run_id`.
    pub fn dir_for(&self, run_id: &str) -> io::Result<PathBuf> {
        if run_id.is_empty()
            || run_id.contains('/')
            || run_id.contains('\\')
            || run_id.contains("..")
            || run_id.starts_with('.')
        {
            return Err(bad_run_id(run_id));
        }
        Ok(self.runs_dir.join(run_id).join(BLOBS_DIR))
    }

    /// The file a hash is stored in for `run_id`.
    pub fn path_for(&self, run_id: &str, sha256: &str) -> io::Result<PathBuf> {
        if !is_sha256_hex(sha256) {
            return Err(bad_sha(sha256));
        }
        Ok(self.dir_for(run_id)?.join(sha256))
    }
}

impl BlobStore for FsBlobStore {
    fn put(&self, run_id: &str, blob: &Blob, reg: &MediaRegistry) -> io::Result<BlobRef> {
        let r = blob.describe(reg);
        let path = self.path_for(run_id, &r.sha256)?;
        if path.is_file() {
            return Ok(r);
        }
        if let Some(dir) = path.parent() {
            leviath_sys::create_private_dir_all(dir)?;
        }
        leviath_sys::write_atomic(&path, &blob.bytes, Some(0o600))?;
        Ok(r)
    }

    fn read(&self, run_id: &str, sha256: &str) -> io::Result<Arc<[u8]>> {
        let path = self.path_for(run_id, sha256)?;
        let bytes = std::fs::read(&path)?;
        Ok(Arc::from(bytes))
    }

    fn copy(&self, from_run: &str, to_run: &str, sha256: &str) -> io::Result<()> {
        let from = self.path_for(from_run, sha256)?;
        let to = self.path_for(to_run, sha256)?;
        if to.is_file() {
            return Ok(());
        }
        let bytes = std::fs::read(&from)?;
        if let Some(dir) = to.parent() {
            leviath_sys::create_private_dir_all(dir)?;
        }
        leviath_sys::write_atomic(&to, &bytes, Some(0o600))
    }

    fn list(&self, run_id: &str) -> io::Result<Vec<String>> {
        let dir = self.dir_for(run_id)?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| is_sha256_hex(name))
            .collect();
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::media::MediaType;

    fn png_blob() -> Blob {
        Blob::new(
            MediaType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nbody".to_vec(),
        )
        .named("a.png")
    }

    #[test]
    fn stores_reads_copies_and_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path().to_path_buf());
        let reg = MediaRegistry::builtin();
        let r = store.put("run-a", &png_blob(), &reg).unwrap();
        let again = store.put("run-a", &png_blob(), &reg).unwrap();
        assert_eq!(r, again);
        let path = store.path_for("run-a", &r.sha256).unwrap();
        assert!(path.is_file());
        assert_eq!(path.parent().unwrap().file_name().unwrap(), BLOBS_DIR);
        assert_eq!(
            &*store.read("run-a", &r.sha256).unwrap(),
            png_blob().bytes.as_slice()
        );
        assert!(store.has("run-a", &r.sha256));
        assert!(!store.has("run-b", &r.sha256));
        assert_eq!(store.list("run-a").unwrap(), vec![r.sha256.clone()]);
        assert!(store.list("run-b").unwrap().is_empty());
        store.copy("run-a", "run-b", &r.sha256).unwrap();
        store.copy("run-a", "run-b", &r.sha256).unwrap();
        assert!(store.has("run-b", &r.sha256));
        assert!(store.copy("run-c", "run-d", &r.sha256).is_err());
        // A stray file that is not a hash is not listed.
        std::fs::write(store.dir_for("run-a").unwrap().join("notes.txt"), b"x").unwrap();
        assert_eq!(store.list("run-a").unwrap().len(), 1);
        assert!(format!("{store:?}").contains("FsBlobStore"));
    }

    #[test]
    fn refuses_keys_that_could_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path().to_path_buf());
        for bad in ["", "../x", "a/b", "a\\b", ".hidden", "x..y"] {
            let err = store.dir_for(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad}");
            assert!(err.to_string().contains("run id"));
        }
        let err = store.read("run-a", "../../etc/passwd").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("sha256"));
        let missing = store.read("run-a", &"0".repeat(64)).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        // A blobs path that is a file, not a directory, is an error on list.
        std::fs::create_dir_all(tmp.path().join("run-f")).unwrap();
        std::fs::write(tmp.path().join("run-f").join(BLOBS_DIR), b"x").unwrap();
        assert!(store.list("run-f").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn blob_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path().to_path_buf());
        let r = store
            .put("run-a", &png_blob(), &MediaRegistry::builtin())
            .unwrap();
        let path = store.path_for("run-a", &r.sha256).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let dir = store.dir_for("run-a").unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(windows)]
    #[test]
    fn blob_files_are_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path().to_path_buf());
        let r = store
            .put("run-a", &png_blob(), &MediaRegistry::builtin())
            .unwrap();
        let path = store.path_for("run-a", &r.sha256).unwrap();
        assert!(path.is_file());
        assert!(store.dir_for("run-a").unwrap().is_dir());
    }

    #[test]
    fn store_for_picks_by_runs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = MediaRegistry::builtin();
        let fs = store_for(Some(tmp.path()));
        let r = fs.put("run-a", &png_blob(), &reg).unwrap();
        assert!(
            tmp.path()
                .join("run-a")
                .join(BLOBS_DIR)
                .join(&r.sha256)
                .is_file()
        );
        let mem = store_for(None);
        let r2 = mem.put("run-a", &png_blob(), &reg).unwrap();
        assert_eq!(r, r2);
        assert!(mem.has("run-a", &r2.sha256));
        let handle = BlobStoreHandle(mem.clone());
        assert!(handle.0.has("run-a", &r2.sha256));
        let reg_handle = MediaRegistryHandle::default();
        assert!(reg_handle.0.row("image/png").is_some());
        let cloned = reg_handle.clone();
        assert!(Arc::ptr_eq(&cloned.0, &reg_handle.0));
        let limits = MediaLimits::default();
        assert_eq!(limits.max_part_bytes, 32 * 1024 * 1024);
        assert_eq!(limits.inline_text_bytes, 1024 * 1024);
        assert_eq!(limits.max_stored_per_request, 100);
        assert_eq!(limits, limits);
        assert!(format!("{limits:?}").contains("MediaLimits"));
    }
}
