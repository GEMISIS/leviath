//! A run's stored parts, read from its files on disk: what `lev blobs` lists
//! and `GET /api/runs/{id}/blobs` serves.
//!
//! A run's context names every stored part by hash, and so does its run
//! file; the bytes are beside it, under `<run>/blobs/<sha256>`, and nowhere
//! else. Nothing here asks the daemon, so a run that finished last week
//! answers as readily as one still going.

use std::collections::BTreeMap;
use std::path::PathBuf;

use leviath_core::mime::{MimeRegistry, MimeType};
use leviath_runtime::spec::names::Digest;
use serde::{Deserialize, Serialize};

use crate::runstate;

/// One stored part a run holds, as the listing shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BlobEntry {
    /// The store's key: the bytes' sha256.
    pub(crate) sha256: String,
    /// The type the bytes were stored as.
    pub(crate) mime_type: String,
    /// The name the part carries, when the context gave it one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// Size in bytes.
    pub(crate) size: u64,
    /// Pixel width, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) width: Option<u32>,
    /// Pixel height, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<u32>,
    /// Duration in milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    /// The token estimate the part is budgeted at.
    pub(crate) tokens: usize,
    /// Every region an entry naming the part sits in, first appearance first.
    pub(crate) regions: Vec<String>,
    /// Whether the bytes are on disk. A context can name a part whose file
    /// was too large to keep, or that a pruned run directory lost.
    pub(crate) stored: bool,
}

impl BlobEntry {
    /// A file name to export the part under: its own name, or its short hash
    /// with the extension the registry gives its type.
    pub(crate) fn file_name(&self, registry: &MimeRegistry) -> String {
        export_name(
            self.name.as_deref(),
            &self.sha256,
            &self.mime_type,
            registry,
        )
    }

    /// The dimensions or duration, as a short label for a listing column.
    pub(crate) fn shape(&self) -> String {
        match (self.width, self.height, self.duration_ms) {
            (Some(w), Some(h), _) => format!("{w}x{h}"),
            (_, _, Some(ms)) => format!("{:.1}s", ms as f64 / 1000.0),
            _ => String::new(),
        }
    }
}

/// A file name to export a stored part under: the name it carries, or its
/// short hash with the extension the registry gives its type.
pub(crate) fn export_name(
    name: Option<&str>,
    sha256: &str,
    mime_type: &str,
    registry: &MimeRegistry,
) -> String {
    if let Some(name) = name {
        return name.to_string();
    }
    let stem: String = sha256.chars().take(12).collect();
    let extension = MimeType::parse(mime_type)
        .ok()
        .and_then(|t| registry.info(&t).extensions.first().cloned());
    match extension {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
}

/// Where the run `run_id` keeps the bytes of the part hashed `sha256`.
pub(crate) fn blob_path(run_id: &str, sha256: &str) -> PathBuf {
    runstate::run_dir(run_id)
        .join(leviath_core::files::BLOBS_DIR)
        .join(sha256)
}

/// The stored parts a run's context holds, by hash, first appearance first,
/// each marked stored when its file is in the run's blob directory. `None`
/// when the run has no file to read.
pub(crate) fn list(run_id: &str) -> Option<Vec<BlobEntry>> {
    let tail = runstate::run_file::tail_in(&runstate::run_dir(run_id)).ok()?;
    let snapshot = leviath_runtime::runfile::context_snapshot(&tail.spec, &tail.state);
    Some(list_from(run_id, &snapshot))
}

/// The stored parts `snapshot` holds, by hash, first appearance first, with
/// whether the run `run_id` keeps each one's bytes in its blob directory.
pub(crate) fn list_from(
    run_id: &str,
    snapshot: &leviath_core::run_meta::ContextSnapshot,
) -> Vec<BlobEntry> {
    entries(snapshot, |sha| blob_path(run_id, sha).is_file())
}

/// The stored parts `snapshot` holds, by hash, first appearance first, each
/// marked stored as `stored` says of its hash.
fn entries(
    snapshot: &leviath_core::run_meta::ContextSnapshot,
    stored: impl Fn(&str) -> bool,
) -> Vec<BlobEntry> {
    let mut order: Vec<String> = Vec::new();
    let mut found: BTreeMap<String, BlobEntry> = BTreeMap::new();
    for region in &snapshot.regions {
        for entry in &region.entries {
            for (part, blob) in entry
                .content
                .stored()
                .filter_map(|p| p.blob().map(|b| (p, b)))
            {
                match found.get_mut(&blob.sha256) {
                    Some(existing) => {
                        if !existing.regions.contains(&region.name) {
                            existing.regions.push(region.name.clone());
                        }
                        if existing.name.is_none() {
                            existing.name = part.name.clone();
                        }
                    }
                    None => {
                        order.push(blob.sha256.clone());
                        found.insert(
                            blob.sha256.clone(),
                            BlobEntry {
                                sha256: blob.sha256.clone(),
                                mime_type: blob.mime_type.to_string(),
                                name: part.name.clone(),
                                size: blob.size,
                                width: blob.width,
                                height: blob.height,
                                duration_ms: blob.duration_ms,
                                tokens: charged_tokens(blob),
                                regions: vec![region.name.clone()],
                                stored: stored(&blob.sha256),
                            },
                        );
                    }
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|sha| found.remove(&sha))
        .collect()
}

/// What a region charges the part: its one-line stand-in, the same figure
/// the runtime budgets it at. The native estimate the registry made at
/// ingest is what a model that takes the part natively pays per request,
/// and showing that here (2.4 million "tokens" for a 9 MB mesh) read as the
/// part's cost to the run, which it is not. A part stored before stand-ins
/// were recorded falls back to the native figure.
fn charged_tokens(blob: &leviath_core::mime::BlobRef) -> usize {
    match blob.stand_in.is_empty() {
        true => blob.tokens,
        false => leviath_core::estimate_tokens(&blob.stand_in),
    }
}

/// How much of a hash a caller has to type for it to count as naming a
/// part. Shorter, and `a` would match half the store.
const MIN_SHA_PREFIX: usize = 6;

/// The listed part `needle` names: by its name first, then by its hash or a
/// prefix of it. A prefix has to be at least [`MIN_SHA_PREFIX`] characters
/// and match exactly one part.
pub(crate) fn find<'a>(entries: &'a [BlobEntry], needle: &str) -> Option<&'a BlobEntry> {
    if let Some(named) = entries.iter().find(|e| e.name.as_deref() == Some(needle)) {
        return Some(named);
    }
    if needle.len() < MIN_SHA_PREFIX {
        return None;
    }
    let mut by_hash = entries.iter().filter(|e| e.sha256.starts_with(needle));
    let first = by_hash.next()?;
    match by_hash.next() {
        Some(_) => None,
        None => Some(first),
    }
}

/// The bytes of the part hashed `sha256`, refusing a key that is not a hash
/// before it touches a path. A part whose file is missing from the run's
/// blob directory is `NotFound`, naming the part.
pub(crate) fn read(run_id: &str, sha256: &str) -> std::io::Result<Vec<u8>> {
    let digest = Digest::new(sha256).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("'{sha256}' is not a sha256"),
        )
    })?;
    leviath_runtime::runfile::read_blob(&runstate::run_dir(run_id), &digest).map_err(|why| {
        let kind = match why {
            leviath_runtime::runfile::RunFileErrorKind::MissingBlob(_) => {
                std::io::ErrorKind::NotFound
            }
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, format!("run '{run_id}' {why}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::{Blob, BlobStore, MimeRegistry, MimeType};

    fn entry(name: Option<&str>, sha: &str) -> BlobEntry {
        BlobEntry {
            sha256: sha.to_string(),
            mime_type: "image/png".to_string(),
            name: name.map(str::to_string),
            size: 3,
            width: None,
            height: None,
            duration_ms: None,
            tokens: 1,
            regions: vec!["task".to_string()],
            stored: true,
        }
    }

    #[test]
    fn a_part_is_found_by_name_then_by_a_unique_hash_prefix() {
        let entries = vec![
            entry(Some("hero.png"), &"a".repeat(64)),
            entry(None, &format!("abcdef{}", "c".repeat(58))),
            entry(None, &format!("abcdef{}", "d".repeat(58))),
        ];
        assert_eq!(find(&entries, "hero.png").unwrap().sha256, "a".repeat(64));
        assert_eq!(
            find(&entries, &"a".repeat(64)).unwrap().sha256,
            "a".repeat(64)
        );
        assert_eq!(
            find(&entries, "abcdefc").unwrap().sha256,
            format!("abcdef{}", "c".repeat(58))
        );
        // Too short to be a hash, and a prefix two parts share.
        assert!(find(&entries, "abc").is_none());
        assert!(find(&entries, "").is_none());
        assert!(find(&entries, "aaaaaa").is_some());
        assert!(find(&entries, "abcdef").is_none());
        assert!(find(&entries, "zzzzzz").is_none());
    }

    #[test]
    fn an_export_name_is_the_part_name_or_the_hash_with_an_extension() {
        let registry = MimeRegistry::builtin();
        let sha = "0123456789abcdef".repeat(4);
        assert_eq!(
            entry(Some("hero.png"), &sha).file_name(&registry),
            "hero.png"
        );
        assert_eq!(entry(None, &sha).file_name(&registry), "0123456789ab.png");
        let mut odd = entry(None, &sha);
        odd.mime_type = "application/x-unknown-thing".to_string();
        assert_eq!(odd.file_name(&registry), "0123456789ab");
        odd.mime_type = "not a type".to_string();
        assert_eq!(odd.file_name(&registry), "0123456789ab");
    }

    /// The listing shows what a region charges the part - its stand-in - not
    /// the native estimate a model that takes it pays; a part stored before
    /// stand-ins were recorded shows the native figure.
    #[test]
    fn a_part_is_listed_at_the_tokens_its_region_charges() {
        let mut blob = leviath_core::mime::BlobRef {
            sha256: "a".repeat(64),
            mime_type: MimeType::parse("model/gltf-binary").unwrap(),
            size: 9_000_000,
            width: None,
            height: None,
            duration_ms: None,
            tokens: 2_400_000,
            stand_in: "[model/gltf-binary 8.6 MB] scene.glb".to_string(),
        };
        assert_eq!(
            charged_tokens(&blob),
            leviath_core::estimate_tokens(&blob.stand_in)
        );
        blob.stand_in.clear();
        assert_eq!(charged_tokens(&blob), 2_400_000);
    }

    #[test]
    fn a_shape_is_dimensions_then_duration_then_nothing() {
        let mut e = entry(None, "x");
        assert_eq!(e.shape(), "");
        e.duration_ms = Some(1500);
        assert_eq!(e.shape(), "1.5s");
        e.width = Some(4);
        e.height = Some(3);
        assert_eq!(e.shape(), "4x3");
    }

    #[test]
    fn bytes_are_read_by_hash_only() {
        runstate::with_isolated_runs_dir("blobs-read", |_d| {
            let run_id = "blob-run";
            runstate::create_run(&crate::test_support::fixtures::run_meta(run_id)).unwrap();
            let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
            let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]);
            let sha = store
                .put(run_id, &blob, &MimeRegistry::builtin())
                .unwrap()
                .sha256;
            assert_eq!(read(run_id, &sha).unwrap(), vec![1, 2, 3]);
            assert_eq!(
                read(run_id, "../meta.json").unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert!(read(run_id, &"f".repeat(64)).is_err());
            // A window with nothing stored in it lists nothing; a run with
            // no file has no window to list.
            assert!(list(run_id).unwrap().is_empty());
            assert!(list("ghost").is_none());
        });
    }

    /// A part whose file is gone from the run's blob directory is listed as
    /// not stored and refused by name; the run's file never holds its bytes.
    #[test]
    fn a_part_whose_file_is_gone_is_listed_unstored_and_refused_by_name() {
        use leviath_core::mime::Part;
        runstate::with_isolated_runs_dir("blobs-gone", |_d| {
            let run_id = "gone-blob-run";
            runstate::create_run(&crate::test_support::fixtures::run_meta(run_id)).unwrap();
            let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
            let blob = Blob::new(MimeType::parse("text/plain").unwrap(), b"notes".to_vec());
            let stored = store.put(run_id, &blob, &MimeRegistry::builtin()).unwrap();
            let sha = stored.sha256.clone();
            let entry = runstate::RegionEntrySnapshot {
                content: leviath_core::region::EntryContent::from_parts(vec![
                    Part::stored(stored).named("notes.txt"),
                ]),
                tokens: 1,
                kind: Default::default(),
                metadata: None,
                key: None,
                taint: leviath_core::taint::TaintLevel::Public,
                reasoning: None,
            };
            let snapshot = runstate::ContextSnapshot {
                stage_name: "main".into(),
                total_tokens: 1,
                max_tokens: 100,
                regions: vec![runstate::RegionSnapshot {
                    name: "task".into(),
                    kind: "pinned".into(),
                    current_tokens: 1,
                    max_tokens: 100,
                    entries: vec![entry],
                    description: None,
                }],
            };
            runstate::write_context_snapshot(run_id, &snapshot).unwrap();
            let listed = list(run_id).unwrap();
            assert_eq!(listed[0].name.as_deref(), Some("notes.txt"));
            assert!(listed[0].stored);
            assert_eq!(read(run_id, &sha).unwrap(), b"notes");

            let dir = runstate::run_dir(run_id);
            std::fs::remove_dir_all(dir.join(leviath_core::files::BLOBS_DIR)).unwrap();
            assert!(!list(run_id).unwrap()[0].stored, "nothing holds the bytes");
            let gone = read(run_id, &sha).unwrap_err();
            assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
            assert!(gone.to_string().contains(&sha), "{gone}");
            // A path that is not a file fails as it is.
            std::fs::create_dir_all(blob_path(run_id, &sha)).unwrap();
            assert_eq!(
                read(run_id, &sha).unwrap_err().kind(),
                std::io::ErrorKind::Other
            );
        });
    }
}
