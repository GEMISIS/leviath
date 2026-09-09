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

/// The three media resources a system reads, as one parameter.
///
/// Every `PipelineWorld` installs all three; a world assembled by hand in a
/// test may install none, and then [`Self::hydration_inputs`] says so and
/// stored parts go out as their stand-ins.
#[derive(bevy_ecs::system::SystemParam)]
pub struct MediaParams<'w> {
    /// The run's blob store.
    pub store: Option<bevy_ecs::system::Res<'w, BlobStoreHandle>>,
    /// The registry that types parts.
    pub registry: Option<bevy_ecs::system::Res<'w, MediaRegistryHandle>>,
    /// The operator's ceilings.
    pub limits: Option<bevy_ecs::system::Res<'w, MediaLimits>>,
}

/// The store and registry a job hydrates with, when both are installed.
pub type HydrationSources = Option<(Arc<dyn BlobStore>, Arc<MediaRegistry>)>;

impl MediaParams<'_> {
    /// The store and registry together when both are installed, and the
    /// per-request cap either way.
    pub fn hydration_inputs(&self) -> (HydrationSources, usize) {
        let both = self
            .store
            .as_deref()
            .map(|s| s.0.clone())
            .zip(self.registry.as_deref().map(|r| r.0.clone()));
        let max_stored = self
            .limits
            .as_deref()
            .map_or(MediaLimits::default().max_stored_per_request, |l| {
                l.max_stored_per_request
            });
        (both, max_stored)
    }

    /// The largest part any ingress accepts.
    pub fn max_part_bytes(&self) -> u64 {
        self.limits
            .as_deref()
            .map_or(MediaLimits::default().max_part_bytes, |l| l.max_part_bytes)
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
        let dir = self.dir_for(run_id)?;
        let path = dir.join(&r.sha256);
        if path.is_file() {
            return Ok(r);
        }
        leviath_sys::create_private_dir_all(&dir)?;
        leviath_sys::write_atomic(&path, &blob.bytes, Some(0o600))?;
        Ok(r)
    }

    fn read(&self, run_id: &str, sha256: &str) -> io::Result<Arc<[u8]>> {
        let path = self.path_for(run_id, sha256)?;
        let bytes = std::fs::read(&path)?;
        Ok(Arc::from(bytes))
    }

    fn copy(&self, from_run: &str, to_run: &str, sha256: &str) -> io::Result<()> {
        // `path_for` has validated the hash, so joining it below is safe.
        let from = self.path_for(from_run, sha256)?;
        let dir = self.dir_for(to_run)?;
        let to = dir.join(sha256);
        if to.is_file() {
            return Ok(());
        }
        let bytes = std::fs::read(&from)?;
        leviath_sys::create_private_dir_all(&dir)?;
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
        assert!(store.list("../x").is_err());
        // The same bad ids are refused on every operation, before any I/O.
        let reg = MediaRegistry::builtin();
        let good = "0".repeat(64);
        assert!(store.put("../x", &png_blob(), &reg).is_err());
        assert!(store.read("../x", &good).is_err());
        assert!(store.copy("../x", "run-a", &good).is_err());
        assert!(store.copy("run-a", "../x", &good).is_err());
        // A blobs directory that cannot be created, because a file sits where
        // it would go, fails the write rather than the whole daemon.
        assert!(store.put("run-f", &png_blob(), &reg).is_err());
        let r = store.put("run-a", &png_blob(), &reg).unwrap();
        assert!(store.copy("run-a", "run-f", &r.sha256).is_err());
        // A directory sitting where the blob file would be written fails the
        // write too.
        std::fs::create_dir_all(store.dir_for("run-d").unwrap().join(&r.sha256)).unwrap();
        assert!(store.put("run-d", &png_blob(), &reg).is_err());
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
        // The bundled parameter answers from a world that installs the
        // resources, and says "nothing to hydrate with" from one that does not.
        let mut world = bevy_ecs::world::World::new();
        let mut state = bevy_ecs::system::SystemState::<MediaParams>::new(&mut world);
        let (none, cap) = state
            .get(&world)
            .expect("the parameter validates")
            .hydration_inputs();
        assert!(none.is_none());
        assert_eq!(cap, 100);
        world.insert_resource(BlobStoreHandle(mem.clone()));
        world.insert_resource(MediaRegistryHandle::default());
        world.insert_resource(MediaLimits {
            max_stored_per_request: 3,
            ..MediaLimits::default()
        });
        let mut state = bevy_ecs::system::SystemState::<MediaParams>::new(&mut world);
        let (both, cap) = state
            .get(&world)
            .expect("the parameter validates")
            .hydration_inputs();
        assert!(both.is_some());
        assert_eq!(cap, 3);
        assert_eq!(limits.max_part_bytes, 32 * 1024 * 1024);
        assert_eq!(limits.inline_text_bytes, 1024 * 1024);
        assert_eq!(limits.max_stored_per_request, 100);
        assert_eq!(limits, limits);
        assert!(format!("{limits:?}").contains("MediaLimits"));
    }
}
