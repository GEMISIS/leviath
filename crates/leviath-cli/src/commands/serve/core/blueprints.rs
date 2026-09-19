//! Reading blueprints: the copy a run executed, and the one installed now.
//!
//! These are two different questions, and answering the first with the second
//! is how "what did this run do" became unanswerable. A run carries its own
//! snapshot of the manifest it executed, identified by digest, so it answers
//! for itself even after the installed file is edited or deleted.
//!
//! Parsing is cached by digest. Five hundred runs of one blueprint parse it
//! once, and a run whose snapshot matches the installed file shares that one
//! parse with the live listing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use leviath_core::Blueprint;

use super::error::ServeError;
use crate::runstate::RunMeta;

/// Where a blueprint was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlueprintSource {
    /// The run's own copy, written at spawn. What the run executed.
    Snapshot,
    /// The installed file, because the run has no snapshot. Runs from before
    /// snapshots existed, where the installed file may have changed since.
    Installed,
}

/// A manifest as read, before it is parsed.
#[derive(Debug)]
pub(crate) struct ManifestText {
    /// The manifest source, verbatim.
    pub(crate) text: String,
    /// Lowercase hex SHA-256 of `text`: the blueprint's identity.
    pub(crate) digest: String,
    /// Which file this came from.
    pub(crate) source: BlueprintSource,
}

/// Read the manifest a run executed.
///
/// The run's snapshot when it has one, and the installed file otherwise. A run
/// with neither cannot be answered for, which is a `NOT_FOUND` about the
/// blueprint rather than about the run: the run is right there, and what is
/// missing is the file it names.
pub(crate) fn manifest_for_run(run_dir: &Path, meta: &RunMeta) -> Result<ManifestText, ServeError> {
    let snapshot = run_dir.join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE);
    if let Ok(text) = std::fs::read_to_string(&snapshot) {
        return Ok(ManifestText::snapshot(text));
    }
    let text = std::fs::read_to_string(&meta.agent_path).map_err(|e| {
        ServeError::NotFound(format!(
            "Run '{}' kept no blueprint snapshot, and its installed blueprint at '{}' \
             cannot be read: {e}",
            meta.run_id, meta.agent_path
        ))
    })?;
    Ok(ManifestText {
        digest: digest_of(&text),
        text,
        source: BlueprintSource::Installed,
    })
}

impl ManifestText {
    /// A manifest read from the installed file.
    ///
    /// The blueprint catalogue already holds the text it read while walking
    /// the agent directories, so this takes that text rather than reading the
    /// file a second time.
    pub(crate) fn installed(text: String) -> Self {
        Self {
            digest: digest_of(&text),
            text,
            source: BlueprintSource::Installed,
        }
    }

    /// A manifest read from a run's own snapshot.
    pub(crate) fn snapshot(text: String) -> Self {
        Self {
            digest: digest_of(&text),
            text,
            source: BlueprintSource::Snapshot,
        }
    }
}

/// The identity of a manifest: the SHA-256 of its bytes, lowercase hex.
///
/// The same function the run snapshot is recorded with, so a digest computed
/// here and one recorded at spawn are comparable. That comparison is the whole
/// point of the field: it is how a client tells "this run executed what is
/// installed now" from "this run executed something else".
pub(crate) fn digest_of(text: &str) -> String {
    leviath_core::mime::store::sha256_hex(text.as_bytes())
}

/// Parsed blueprints, kept by digest.
///
/// Content-addressed rather than keyed by name or path, so two runs of the
/// same manifest share one parse however they reached it, and an edited
/// blueprint is a different key rather than a stale entry.
#[derive(Clone, Default)]
pub(crate) struct BlueprintCache {
    parsed: Arc<Mutex<HashMap<String, Arc<Blueprint>>>>,
}

/// How many parsed manifests one server keeps.
///
/// Beyond this the cache is cleared rather than trimmed by age. A blueprint is
/// small, the bound exists so a machine with thousands of distinct manifests
/// cannot grow this without limit, and "clear and refill" costs one parse per
/// blueprint actually in use rather than the bookkeeping a proper eviction
/// order would need.
const MAX_PARSED: usize = 256;

impl BlueprintCache {
    /// The parsed blueprint for this manifest, parsing it the first time.
    ///
    /// A manifest that will not parse is reported rather than cached: the
    /// failure is about this document, and caching it would answer the same
    /// way after the file is fixed.
    pub(crate) fn parse(&self, manifest: &ManifestText) -> Result<Arc<Blueprint>, ServeError> {
        if let Some(hit) = self.lookup(&manifest.digest) {
            return Ok(hit);
        }
        let parsed = leviath_core::manifest::parse_manifest(&manifest.text).map_err(|e| {
            // Internal, not a bad request: whoever asked did not write this
            // file, and on the snapshot path this server wrote it.
            ServeError::Internal(format!("Blueprint will not parse: {e}"))
        })?;
        let parsed = Arc::new(parsed);
        self.store(manifest.digest.clone(), Arc::clone(&parsed));
        Ok(parsed)
    }

    /// The cached parse for a digest, if this server holds one.
    fn lookup(&self, digest: &str) -> Option<Arc<Blueprint>> {
        leviath_core::sync::lock(&self.parsed).get(digest).cloned()
    }

    /// Remember one parse, clearing the cache when it has grown past its
    /// bound.
    fn store(&self, digest: String, parsed: Arc<Blueprint>) {
        let mut cache = leviath_core::sync::lock(&self.parsed);
        if cache.len() >= MAX_PARSED {
            cache.clear();
        }
        cache.insert(digest, parsed);
    }
}

/// The directory a run's files live in.
///
/// Here rather than at each call site because the GraphQL resolvers and the
/// service layer both need it, and `runstate` keeps the one that resolves the
/// home directory private to itself.
pub(crate) fn run_dir(run_id: &str) -> PathBuf {
    crate::runstate::runs_dir().join(run_id)
}

#[cfg(test)]
#[path = "blueprints_tests.rs"]
mod tests;
