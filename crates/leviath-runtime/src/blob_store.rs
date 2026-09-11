//! Where a run's stored mime parts live on disk, and the world resources
//! that hand the store and the mime registry to every system.
//!
//! A part whose bytes are not text is written once under
//! `<runs_dir>/<run_id>/blobs/<sha256>` and referenced by hash everywhere
//! else. Deleting the run deletes its blobs; nothing outside the run's
//! directory points at them. A world with no runs directory (the embedding
//! mode) keeps the same bytes in memory instead.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy_ecs::prelude::{Component, Entity, Resource, World};
use leviath_core::files::BLOBS_DIR;
use leviath_core::mime::registry::RegistryError;
use leviath_core::mime::{
    Blob, BlobRef, BlobStore, MemoryBlobStore, MimeCheck, MimeRegistry, RegistryCell, is_sha256_hex,
};

/// The store every system reads and writes stored parts through.
#[derive(Resource, Clone)]
pub struct BlobStoreHandle(pub Arc<dyn BlobStore>);

/// The mime registry a world was built with: the compiled defaults plus the
/// operator's `[mime_types]`. A blueprint's own rows layer on top per agent.
#[derive(Resource, Clone)]
pub struct MimeRegistryHandle(pub Arc<MimeRegistry>);

impl Default for MimeRegistryHandle {
    fn default() -> Self {
        Self(Arc::new(MimeRegistry::builtin()))
    }
}

/// The registry one run reads: the world's rows with the blueprint's own
/// `[mime_types]` layered on top, and the blueprint's compiled checks
/// attached.
///
/// Built once at spawn and held for the life of the run behind a
/// [`RegistryCell`], which the tool lane shares (see `ToolMime`), so the
/// systems that type a run's bytes and the tools that store them read one
/// registry. When the operator's rows change under a live daemon,
/// [`refresh_run_registries`] rebuilds every run's registry over the new
/// base and stores it into the same cell, and every holder's next read is
/// the edited one.
#[derive(Component, Clone, Debug)]
pub struct RunMimeRegistry {
    /// The blueprint's rows, as written.
    rows: toml::Table,
    /// The blueprint's compiled checks, keyed by row.
    checks: BTreeMap<String, Arc<dyn MimeCheck>>,
    /// Where the built registry lives.
    cell: Arc<RegistryCell>,
}

impl RunMimeRegistry {
    /// `base` (the world's registry) with `rows` layered on top and `checks`
    /// attached. A row that will not layer, or a check on a key that is not
    /// a type, is the error: the spawn refuses rather than typing the run's
    /// bytes against half a table.
    pub fn new(
        base: &MimeRegistry,
        rows: toml::Table,
        checks: BTreeMap<String, Arc<dyn MimeCheck>>,
    ) -> Result<Self, RegistryError> {
        let built = Self::build(base, &rows, &checks)?;
        Ok(Self {
            rows,
            checks,
            cell: Arc::new(RegistryCell::new(Arc::new(built))),
        })
    }

    fn build(
        base: &MimeRegistry,
        rows: &toml::Table,
        checks: &BTreeMap<String, Arc<dyn MimeCheck>>,
    ) -> Result<MimeRegistry, RegistryError> {
        let mut registry = base.layered(rows, "blueprint")?;
        for (key, check) in checks {
            registry.attach_check(key, check.clone())?;
        }
        Ok(registry)
    }

    /// The cell the registry lives in, for a holder outside the world.
    pub fn cell(&self) -> Arc<RegistryCell> {
        self.cell.clone()
    }

    /// The registry as it stands now.
    pub fn registry(&self) -> Arc<MimeRegistry> {
        self.cell.load()
    }

    /// Rebuild over a new `base` and swap it in for every holder.
    pub fn rebuild(&self, base: &MimeRegistry) -> Result<(), RegistryError> {
        let built = Self::build(base, &self.rows, &self.checks)?;
        self.cell.store(Arc::new(built));
        Ok(())
    }
}

/// Rebuild every live run's registry over `base`, the world's registry as it
/// stands after a reload, and report how many were rebuilt. A run whose rows
/// no longer layer keeps the registry it had, with a warning; its rows were
/// checked at spawn, so that means the base changed under them.
pub fn refresh_run_registries(world: &mut World, base: &MimeRegistry) -> usize {
    let mut refreshed = 0;
    for (entity, run) in world.query::<(Entity, &RunMimeRegistry)>().iter(world) {
        match run.rebuild(base) {
            Ok(()) => refreshed += 1,
            Err(e) => tracing::warn!(
                ?entity,
                "[mime] a run keeps its old mime registry; its rows no longer layer: {e}"
            ),
        }
    }
    refreshed
}

/// The mime resources a system reads, as one parameter: the world's store,
/// registry and ceilings, and each live run's own registry.
///
/// Every `PipelineWorld` installs the three resources; a world assembled by
/// hand in a test may install none, and then [`Self::hydration_inputs`] says
/// so and stored parts go out as their stand-ins.
#[derive(bevy_ecs::system::SystemParam)]
pub struct MimeParams<'w, 's> {
    /// The run's blob store.
    pub store: Option<bevy_ecs::system::Res<'w, BlobStoreHandle>>,
    /// The registry that types parts, for a run without one of its own.
    pub registry: Option<bevy_ecs::system::Res<'w, MimeRegistryHandle>>,
    /// The operator's ceilings.
    pub limits: Option<bevy_ecs::system::Res<'w, MimeLimits>>,
    /// Each live run's own registry, where its spawn built one.
    pub runs: bevy_ecs::system::Query<'w, 's, &'static RunMimeRegistry>,
}

/// The store and registry a job hydrates with, when both are installed.
pub type HydrationSources = Option<(Arc<dyn BlobStore>, Arc<MimeRegistry>)>;

impl MimeParams<'_, '_> {
    /// The registry `entity`'s run reads: its own when its spawn built one,
    /// else the world's. `None` in a world with neither.
    pub fn registry_for(&self, entity: Entity) -> Option<Arc<MimeRegistry>> {
        self.runs
            .get(entity)
            .ok()
            .map(RunMimeRegistry::registry)
            .or_else(|| self.registry.as_deref().map(|r| r.0.clone()))
    }

    /// The store and the registry `entity`'s run reads, together when both
    /// are there, and the per-request media-byte cap either way.
    pub fn hydration_inputs(&self, entity: Entity) -> (HydrationSources, u64) {
        let both = self
            .store
            .as_deref()
            .map(|s| s.0.clone())
            .zip(self.registry_for(entity));
        let max_media_bytes = self
            .limits
            .as_deref()
            .map_or(MimeLimits::default().max_media_bytes_per_request, |l| {
                l.max_media_bytes_per_request
            });
        (both, max_media_bytes)
    }

    /// The largest part any ingress accepts.
    pub fn max_part_bytes(&self) -> u64 {
        self.limits
            .as_deref()
            .map_or(MimeLimits::default().max_part_bytes, |l| l.max_part_bytes)
    }
}

/// The operator's ceilings on typed parts, from `[mime]` in the config.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MimeLimits {
    /// Bytes one part may be; larger is refused wherever it arrives.
    pub max_part_bytes: u64,
    /// Bytes of text kept inline in an entry before the part is stored.
    pub inline_text_bytes: u64,
    /// Bytes of stored media one request carries before the oldest are sent as
    /// stand-ins.
    pub max_media_bytes_per_request: u64,
}

impl Default for MimeLimits {
    fn default() -> Self {
        Self {
            max_part_bytes: 32 * 1024 * 1024,
            inline_text_bytes: 1024 * 1024,
            max_media_bytes_per_request: 64 * 1024 * 1024,
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
    fn put(&self, run_id: &str, blob: &Blob, reg: &MimeRegistry) -> io::Result<BlobRef> {
        leviath_core::mime::verify_blob(reg, blob)?;
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
    use leviath_core::mime::MimeType;

    fn png_blob() -> Blob {
        Blob::new(
            MimeType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nbody".to_vec(),
        )
        .named("a.png")
    }

    #[test]
    fn stores_reads_copies_and_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path().to_path_buf());
        let reg = MimeRegistry::builtin();
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

    /// A check a row names runs where the bytes are written, so an on-disk
    /// store refuses bytes that fail it and writes nothing.
    #[test]
    fn a_failed_check_writes_nothing() {
        use leviath_core::mime::FnCheck;
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path().to_path_buf());
        let mut reg = MimeRegistry::builtin();
        let rows: toml::Table = toml::from_str("[\"image/png\"]\ncheck = \"png.rhai\"\n").unwrap();
        reg.layer(&rows, "t").unwrap();
        reg.attach_check(
            "image/png",
            Arc::new(FnCheck::new(
                "png",
                |_: &MimeType, bytes: &[u8]| match bytes.starts_with(b"\x89PNG") {
                    true => Ok(()),
                    false => Err("no PNG signature".to_string()),
                },
            )),
        )
        .unwrap();
        let fake =
            Blob::new(MimeType::parse("image/png").unwrap(), b"GIF89a".to_vec()).named("shot.png");
        let err = store.put("run-a", &fake, &reg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("no PNG signature"), "{err}");
        assert!(store.list("run-a").unwrap().is_empty());
        assert!(store.put("run-a", &png_blob(), &reg).is_ok());
    }

    /// A run's registry is the world's rows with the blueprint's on top and
    /// its checks attached; a rebuild over a new base reaches the cell every
    /// holder shares, and a base that no longer takes the rows is refused.
    #[test]
    fn a_run_registry_layers_the_blueprint_and_follows_a_rebuild() {
        use leviath_core::mime::FnCheck;
        let base = MimeRegistry::builtin();
        let rows: toml::Table = toml::from_str(
            "[\"application/x-acme-scene\"]\nfamily = \"model\"\ncheck = \"checks/scene.rhai\"\n",
        )
        .unwrap();
        let mut checks: BTreeMap<String, Arc<dyn MimeCheck>> = BTreeMap::new();
        checks.insert(
            "application/x-acme-scene".to_string(),
            Arc::new(FnCheck::new(
                "scene",
                |_: &MimeType, bytes: &[u8]| match bytes.starts_with(b"ACME") {
                    true => Ok(()),
                    false => Err("missing the ACME tag".to_string()),
                },
            )),
        );
        let run = RunMimeRegistry::new(&base, rows.clone(), checks.clone()).unwrap();
        let scene = MimeType::parse("application/x-acme-scene").unwrap();
        let held = run.cell();
        assert_eq!(run.registry().info(&scene).family, "model");
        assert_eq!(run.registry().info(&scene).source, "blueprint");
        assert_eq!(
            held.load().verify(&scene, b"NOPE"),
            Err("missing the ACME tag".to_string())
        );
        assert_eq!(held.load().verify(&scene, b"ACME\x00"), Ok(()));
        assert!(format!("{run:?}").contains("checks/scene.rhai"));

        // The operator edits the obj row: the rebuild reaches the shared cell
        // and keeps the blueprint's row and check.
        let edited: toml::Table = toml::from_str("[\"model/obj\"]\nfamily = \"scene\"\n").unwrap();
        let next = base.layered(&edited, "mime_types.toml").unwrap();
        run.rebuild(&next).unwrap();
        let obj = MimeType::parse("model/obj").unwrap();
        assert_eq!(held.load().info(&obj).family, "scene");
        assert_eq!(held.load().info(&scene).family, "model");
        assert_eq!(
            held.load().verify(&scene, b"NOPE"),
            Err("missing the ACME tag".to_string())
        );

        // Bad rows are refused at construction, and a check on a key that is
        // not a type is refused too.
        let bad: toml::Table = toml::from_str("[png]\nfamily = \"image\"\n").unwrap();
        assert!(RunMimeRegistry::new(&base, bad, BTreeMap::new()).is_err());
        let mut bad_key: BTreeMap<String, Arc<dyn MimeCheck>> = BTreeMap::new();
        bad_key.insert(
            "png".to_string(),
            checks["application/x-acme-scene"].clone(),
        );
        assert!(RunMimeRegistry::new(&base, rows, bad_key).is_err());

        // Across a world: every run with a registry is rebuilt; one whose
        // rows the new base refuses is left as it was, and counted out.
        let mut world = World::new();
        world.spawn(run.clone());
        world.spawn(());
        assert_eq!(refresh_run_registries(&mut world, &base), 1);
        assert_eq!(held.load().info(&obj).family, "model", "back on the base");
        let stuck = RunMimeRegistry {
            rows: toml::from_str("[png]\nfamily = \"image\"\n").unwrap(),
            checks: BTreeMap::new(),
            cell: Arc::new(RegistryCell::default()),
        };
        world.spawn(stuck);
        assert_eq!(refresh_run_registries(&mut world, &next), 1);
        assert_eq!(held.load().info(&obj).family, "scene");
    }

    /// The parameter answers for a run: its own registry when it has one,
    /// the world's otherwise, and nothing in a world with neither.
    #[test]
    fn mime_params_resolve_a_registry_per_run() {
        let mut world = World::new();
        let bare = world.spawn(()).id();
        let rows: toml::Table = toml::from_str("[\"model/obj\"]\nfamily = \"scene\"\n").unwrap();
        let own = world
            .spawn(RunMimeRegistry::new(&MimeRegistry::builtin(), rows, BTreeMap::new()).unwrap())
            .id();
        let obj = MimeType::parse("model/obj").unwrap();
        {
            let mut state = bevy_ecs::system::SystemState::<MimeParams>::new(&mut world);
            let mime = state.get(&world).unwrap();
            assert!(mime.registry_for(bare).is_none());
            assert_eq!(mime.registry_for(own).unwrap().info(&obj).family, "scene");
            assert!(mime.hydration_inputs(own).0.is_none(), "no store yet");
        }
        world.insert_resource(BlobStoreHandle(Arc::new(MemoryBlobStore::new())));
        world.insert_resource(MimeRegistryHandle::default());
        let mut state = bevy_ecs::system::SystemState::<MimeParams>::new(&mut world);
        let mime = state.get(&world).unwrap();
        assert_eq!(mime.registry_for(bare).unwrap().info(&obj).family, "model");
        let (sources, max) = mime.hydration_inputs(own);
        assert_eq!(sources.unwrap().1.info(&obj).family, "scene");
        assert_eq!(max, MimeLimits::default().max_media_bytes_per_request);
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
        let reg = MimeRegistry::builtin();
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
            .put("run-a", &png_blob(), &MimeRegistry::builtin())
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
            .put("run-a", &png_blob(), &MimeRegistry::builtin())
            .unwrap();
        let path = store.path_for("run-a", &r.sha256).unwrap();
        assert!(path.is_file());
        assert!(store.dir_for("run-a").unwrap().is_dir());
    }

    #[test]
    fn store_for_picks_by_runs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = MimeRegistry::builtin();
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
        let reg_handle = MimeRegistryHandle::default();
        assert!(reg_handle.0.row("image/png").is_some());
        let cloned = reg_handle.clone();
        assert!(Arc::ptr_eq(&cloned.0, &reg_handle.0));
        let limits = MimeLimits::default();
        // The bundled parameter answers from a world that installs the
        // resources, and says "nothing to hydrate with" from one that does not.
        let mut world = bevy_ecs::world::World::new();
        let entity = world.spawn(()).id();
        let mut state = bevy_ecs::system::SystemState::<MimeParams>::new(&mut world);
        let (none, cap) = state
            .get(&world)
            .expect("the parameter validates")
            .hydration_inputs(entity);
        assert!(none.is_none());
        assert_eq!(cap, 64 * 1024 * 1024);
        world.insert_resource(BlobStoreHandle(mem.clone()));
        world.insert_resource(MimeRegistryHandle::default());
        world.insert_resource(MimeLimits {
            max_media_bytes_per_request: 3,
            ..MimeLimits::default()
        });
        let mut state = bevy_ecs::system::SystemState::<MimeParams>::new(&mut world);
        let (both, cap) = state
            .get(&world)
            .expect("the parameter validates")
            .hydration_inputs(entity);
        assert!(both.is_some());
        assert_eq!(cap, 3);
        assert_eq!(limits.max_part_bytes, 32 * 1024 * 1024);
        assert_eq!(limits.inline_text_bytes, 1024 * 1024);
        assert_eq!(limits.max_media_bytes_per_request, 64 * 1024 * 1024);
        assert_eq!(limits, limits);
        assert!(format!("{limits:?}").contains("MimeLimits"));
    }
}
