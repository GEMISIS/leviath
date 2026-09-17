//! Parts a run put in a vendor's file storage: uploaded once, named by id on
//! every later request, and deleted when the run ends.
//!
//! The ledger is `<run dir>/provider-files.json`, one entry per provider and
//! part hash. It is the only source of a file id a request ever names: an id
//! from a blueprint or a person could read another tenant's upload, so none is
//! taken from anywhere else. It survives a daemon restart and a resume, and
//! [`forget_run`] empties it (deleting each file) when the run finishes or is
//! deleted. The vendor's own expiry, set on every upload, is the backstop for
//! a daemon that dies before that.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use leviath_providers::files::{FileUpload, RemoteFile, now_secs};
use leviath_providers::{ContentBlock, InferenceRequest, MessageContent, ModelMime, Provider};

/// The ledger's file name inside a run's directory.
pub const LEDGER_FILE: &str = "provider-files.json";

/// A file this close to its expiry is uploaded again rather than named: a
/// long request could outlive it.
const REUSE_MARGIN_SECS: i64 = 600;

/// One uploaded part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The registry name of the provider holding it.
    pub provider: String,
    /// The part's hash.
    pub sha256: String,
    /// The vendor's file.
    pub file: RemoteFile,
}

/// The ledger as stored.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    files: Vec<Entry>,
}

/// The ledger at `path`; empty when there is none or it does not parse.
fn load(path: &Path) -> Ledger {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "an unreadable provider file ledger was started afresh");
            Ledger::default()
        }),
        Err(_) => Ledger::default(),
    }
}

/// Write the ledger, owner-only and whole. A failure is logged: the files
/// still expire at the vendor, and the next upload tries the write again.
fn save(path: &Path, ledger: &Ledger) {
    let written = serde_json::to_vec_pretty(ledger)
        .map_err(std::io::Error::other)
        .and_then(|bytes| leviath_sys::write_private(path, &bytes));
    if let Err(e) = written {
        tracing::warn!(path = %path.display(), error = %e, "the provider file ledger could not be written");
    }
}

/// One lock per ledger, so two requests of one run never upload the same
/// part twice or write the ledger over each other.
fn lock_for(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path.to_path_buf())
        .or_default()
        .clone()
}

/// Where a job's uploads go and how long they live.
#[derive(Clone)]
pub(crate) struct FileRoute {
    /// The provider the request goes to.
    pub provider: Arc<dyn Provider>,
    /// Its registry name, which the ledger keys by.
    pub provider_name: String,
    /// The run's ledger file.
    pub ledger: PathBuf,
    /// The lifetime to ask for, before the vendor's clamp.
    pub ttl_secs: u64,
}

/// Where a job's uploads go, if anywhere, and why parts go inline when they
/// could have been uploaded. Uploads need the switch on, no zero retention,
/// a provider that stores files, and a run directory to record them in, so
/// the run can delete them when it ends.
pub(crate) fn route_for(
    provider: &Arc<dyn Provider>,
    provider_name: &str,
    limits: &leviath_providers::files::MediaLimits,
    settings: &leviath_providers::retention::RetentionSettings,
    store: &dyn leviath_core::mime::BlobStore,
    run_id: &str,
    ttl_secs: u64,
) -> (Option<FileRoute>, &'static str) {
    if limits.file_bytes.is_none() {
        return (None, "");
    }
    let route = match settings.uploads_allowed() {
        true => store.run_dir(run_id).map(|dir| FileRoute {
            provider: provider.clone(),
            provider_name: provider_name.to_string(),
            ledger: dir.join(LEDGER_FILE),
            ttl_secs,
        }),
        false => None,
    };
    (route, settings.why_inline())
}

/// A file name every vendor takes: the part's own name without the
/// characters some refuse, else one made from its hash.
fn file_name(name: Option<&str>, sha256: &str) -> String {
    let cleaned: String = name
        .unwrap_or_default()
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(200)
        .collect();
    match cleaned.trim() {
        "" => format!("part-{}", sha256.get(..12).unwrap_or(sha256)),
        name => name.to_string(),
    }
}

/// Name, by the vendor's copy, every part of `request` this provider can
/// take by file: a copy the ledger holds, or a fresh upload. `again` uploads
/// every part already named, for a request the vendor answered with a file it
/// no longer has. A part whose bytes cannot be read, or whose upload fails,
/// stays as it was and goes inline. Returns how many parts were uploaded.
pub(crate) async fn attach(
    request: &mut InferenceRequest,
    route: &FileRoute,
    mime: &ModelMime,
    store: &dyn leviath_core::mime::BlobStore,
    run_id: &str,
    again: bool,
) -> usize {
    let limits = route.provider.media_limits(&request.model);
    if limits.file_bytes.is_none() {
        return 0;
    }
    let lock = lock_for(&route.ledger);
    let _held = lock.lock().await;
    let mut ledger = load(&route.ledger);
    let now = now_secs();
    let mut uploaded = 0;
    let mut renewed: Vec<String> = Vec::new();
    for message in &mut request.messages {
        let MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        for block in blocks.iter_mut() {
            let native = leviath_providers::mime::sends_natively(block, mime);
            let ContentBlock::Mime {
                part, name, remote, ..
            } = block
            else {
                continue;
            };
            if !native
                || !limits.by_file(&part.mime_type, part.size)
                || (remote.is_some() && !again)
                || (remote.is_none() && again)
            {
                continue;
            }
            let held = ledger.files.iter().find(|e| {
                e.provider == route.provider_name
                    && e.sha256 == part.sha256
                    && e.file.usable(now, REUSE_MARGIN_SECS)
            });
            // A part named again in this pass was uploaded a moment ago, and
            // its entry is the fresh one.
            let reusable = !again || renewed.contains(&part.sha256);
            if let (Some(entry), true) = (held, reusable) {
                *remote = Some(entry.file.clone());
                continue;
            }
            let Ok(bytes) = store.read(run_id, &part.sha256) else {
                continue;
            };
            let upload = FileUpload {
                bytes,
                mime_type: part.mime_type.to_string(),
                name: file_name(name.as_deref(), &part.sha256),
                ttl_secs: route.ttl_secs,
            };
            match route.provider.upload_file(&upload).await {
                Ok(file) => {
                    ledger.files.retain(|e| {
                        !(e.provider == route.provider_name && e.sha256 == part.sha256)
                    });
                    ledger.files.push(Entry {
                        provider: route.provider_name.clone(),
                        sha256: part.sha256.clone(),
                        file: file.clone(),
                    });
                    renewed.push(part.sha256.clone());
                    *remote = Some(file);
                    uploaded += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %route.provider_name,
                        part = %part.sha256,
                        error = %e,
                        "a part could not be uploaded and goes inline"
                    );
                    *remote = None;
                }
            }
        }
    }
    if uploaded > 0 {
        save(&route.ledger, &ledger);
        tracing::info!(provider = %route.provider_name, uploaded, "[mime] parts uploaded to provider file storage");
    }
    uploaded
}

/// Whether `request` names any vendor copy.
pub(crate) fn names_files(request: &InferenceRequest) -> bool {
    request.messages.iter().any(|m| match &m.content {
        MessageContent::Blocks(blocks) => blocks.iter().any(|b| {
            matches!(
                b,
                ContentBlock::Mime {
                    remote: Some(_),
                    ..
                }
            )
        }),
        MessageContent::Text(_) => false,
    })
}

/// Take the ledger of the run at `run_dir`: its entries, with the file
/// removed so nothing names them again. What a caller about to delete the
/// run's directory reads first, then hands to [`delete_entries`].
pub fn take_ledger(run_dir: &Path) -> Vec<Entry> {
    let path = run_dir.join(LEDGER_FILE);
    if !path.is_file() {
        return Vec::new();
    }
    let ledger = load(&path);
    // A ledger left behind names files already deleted, and a second delete
    // of one is answered as done, so a failed removal costs nothing.
    let _ = std::fs::remove_file(&path);
    ledger.files
}

/// Delete `entries` at their providers. Best effort: a provider no longer
/// configured, or a delete that fails, is logged and left to the vendor's
/// expiry. Returns how many files were deleted.
pub async fn delete_entries(entries: &[Entry], registry: &crate::ProviderRegistry) -> usize {
    let mut deleted = 0;
    for entry in entries {
        let Some(provider) = registry.get(&entry.provider) else {
            tracing::info!(provider = %entry.provider, file = %entry.file.id, "a provider no longer configured keeps its upload until it expires");
            continue;
        };
        match provider.delete_file(&entry.file).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                tracing::warn!(provider = %entry.provider, file = %entry.file.id, error = %e, "an uploaded file could not be deleted; it expires at the vendor")
            }
        }
    }
    deleted
}

/// Delete every file the run at `run_dir` uploaded, and its ledger. Returns
/// how many files were deleted.
pub async fn forget_run(run_dir: &Path, registry: &crate::ProviderRegistry) -> usize {
    let lock = lock_for(&run_dir.join(LEDGER_FILE));
    let entries = {
        let _held = lock.lock().await;
        take_ledger(run_dir)
    };
    delete_entries(&entries, registry).await
}

/// For a daemon's reap hook: when the agent at `entity` has finished (not
/// merely parked while paused, which keeps its files for the resume), delete
/// what its run uploaded, in the background. Whether a delete was started.
pub fn forget_finished(world: &bevy_ecs::world::World, entity: bevy_ecs::entity::Entity) -> bool {
    let Some(state) = world.get::<crate::components::AgentState>(entity) else {
        return false;
    };
    if !crate::pipeline::is_terminal_status(&state.status) {
        return false;
    }
    let (Some(store), Some(providers)) = (
        world.get_resource::<crate::blob_store::BlobStoreHandle>(),
        world.get_resource::<crate::pipeline::Providers>(),
    ) else {
        return false;
    };
    let Some(run_dir) = store.0.run_dir(&state.agent_id) else {
        return false;
    };
    forget_in_background(run_dir, &providers.0)
}

/// [`forget_run`] on the running runtime, without waiting, when the run at
/// `run_dir` has a ledger. Whether a delete was started.
pub fn forget_in_background(run_dir: PathBuf, registry: &crate::ProviderRegistry) -> bool {
    if !run_dir.join(LEDGER_FILE).is_file() {
        return false;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    let registry = registry.clone();
    runtime.spawn(async move {
        let deleted = forget_run(&run_dir, &registry).await;
        tracing::info!(deleted, "[mime] a finished run's uploads were deleted");
    });
    true
}

#[cfg(test)]
mod tests;
