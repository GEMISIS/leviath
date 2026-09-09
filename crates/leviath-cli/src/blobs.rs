//! A run's stored parts, read from its files on disk: what `lev blobs` lists
//! and `GET /api/agents/{id}/blobs` serves.
//!
//! A run's context snapshot names every stored part by hash, and the bytes
//! sit under `<run>/blobs/<sha256>`. Nothing here asks the daemon, so a run
//! that finished last week answers as readily as one still going.

use std::collections::BTreeMap;
use std::path::PathBuf;

use leviath_core::media::{MediaRegistry, MediaType, is_sha256_hex};
use serde::{Deserialize, Serialize};

use crate::runstate;

/// One stored part a run holds, as the listing shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BlobEntry {
    /// The store's key: the bytes' sha256.
    pub(crate) sha256: String,
    /// The type the bytes were stored as.
    pub(crate) media_type: String,
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
    pub(crate) fn file_name(&self, registry: &MediaRegistry) -> String {
        export_name(
            self.name.as_deref(),
            &self.sha256,
            &self.media_type,
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
    media_type: &str,
    registry: &MediaRegistry,
) -> String {
    if let Some(name) = name {
        return name.to_string();
    }
    let stem: String = sha256.chars().take(12).collect();
    let extension = MediaType::parse(media_type)
        .ok()
        .and_then(|t| registry.info(&t).extensions.first().cloned());
    match extension {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
}

/// Where a run keeps the bytes of the part hashed `sha256`.
pub(crate) fn blob_path(run_id: &str, sha256: &str) -> PathBuf {
    runstate::run_dir(run_id)
        .join(leviath_core::files::BLOBS_DIR)
        .join(sha256)
}

/// The stored parts a run's context holds, by hash, first appearance first.
/// `None` when the run has no context snapshot to read.
pub(crate) fn list(run_id: &str) -> Option<Vec<BlobEntry>> {
    let snapshot = runstate::read_context_snapshot(run_id)?;
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
                                media_type: blob.media_type.to_string(),
                                name: part.name.clone(),
                                size: blob.size,
                                width: blob.width,
                                height: blob.height,
                                duration_ms: blob.duration_ms,
                                tokens: blob.tokens,
                                regions: vec![region.name.clone()],
                                stored: blob_path(run_id, &blob.sha256).is_file(),
                            },
                        );
                    }
                }
            }
        }
    }
    Some(
        order
            .into_iter()
            .filter_map(|sha| found.remove(&sha))
            .collect(),
    )
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
/// before it touches a path.
pub(crate) fn read(run_id: &str, sha256: &str) -> std::io::Result<Vec<u8>> {
    if !is_sha256_hex(sha256) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("'{sha256}' is not a sha256"),
        ));
    }
    std::fs::read(blob_path(run_id, sha256))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::media::{Blob, BlobStore, MediaRegistry, MediaType};

    fn entry(name: Option<&str>, sha: &str) -> BlobEntry {
        BlobEntry {
            sha256: sha.to_string(),
            media_type: "image/png".to_string(),
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
        let registry = MediaRegistry::builtin();
        let sha = "0123456789abcdef".repeat(4);
        assert_eq!(
            entry(Some("hero.png"), &sha).file_name(&registry),
            "hero.png"
        );
        assert_eq!(entry(None, &sha).file_name(&registry), "0123456789ab.png");
        let mut odd = entry(None, &sha);
        odd.media_type = "application/x-unknown-thing".to_string();
        assert_eq!(odd.file_name(&registry), "0123456789ab");
        odd.media_type = "not a type".to_string();
        assert_eq!(odd.file_name(&registry), "0123456789ab");
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
            let blob = Blob::new(MediaType::parse("image/png").unwrap(), vec![1, 2, 3]);
            let sha = store
                .put(run_id, &blob, &MediaRegistry::builtin())
                .unwrap()
                .sha256;
            assert_eq!(read(run_id, &sha).unwrap(), vec![1, 2, 3]);
            assert_eq!(
                read(run_id, "../meta.json").unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert!(read(run_id, &"f".repeat(64)).is_err());
            // No snapshot: nothing to list.
            assert!(list(run_id).is_none());
        });
    }
}
