//! Taking a webhook secret out of the old files a converted run keeps.
//!
//! An old run held its webhook's signing secret as `callback_secret` in
//! `meta.json` and in every journal record that carried the metadata. Once
//! the secret is in the secret store, the copies under `legacy/` go: each
//! file holding the key is written again without it. The new file replaces
//! the old one by a rename, so a hard link to the old file (the home's
//! backup) keeps what it held.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The key an old run kept its webhook secret under.
const KEY: &str = "callback_secret";

/// What a file being written again is called until it replaces the old one:
/// its name with this after it.
const SCRUBBING: &str = ".scrubbing";

/// Take every `callback_secret` out of the files directly in `legacy`: a
/// JSON file, or an LVR1 journal frame by frame. A file that is neither, or
/// holds no such key, is left as it is.
pub(crate) fn secrets(legacy: &Path) -> std::io::Result<()> {
    let files: Vec<PathBuf> = std::fs::read_dir(legacy)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|path| path.is_file())
        .collect();
    files.iter().try_for_each(|path| {
        std::fs::read(path).and_then(|bytes| {
            let scrubbed = match bytes.starts_with(crate::journal::MAGIC) {
                true => journal(&bytes),
                false => json(&bytes),
            };
            scrubbed.map_or(Ok(()), |scrubbed| replace(path, &scrubbed))
        })
    })
}

/// `path` with `bytes` in it, by a new file renamed over it.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut aside = path.as_os_str().to_owned();
    aside.push(SCRUBBING);
    let aside = PathBuf::from(aside);
    leviath_sys::perms::write_private(&aside, bytes).and_then(|()| std::fs::rename(&aside, path))
}

/// A JSON file without the key, or `None` when it is not JSON or never held
/// the key.
fn json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    strip(&mut value).then(|| serde_json::to_vec_pretty(&value).unwrap_or_default())
}

/// An LVR1 journal with the key taken out of every frame, or `None` when no
/// frame held it. A frame that is not JSON, and a torn last frame, are kept
/// byte for byte.
fn journal(bytes: &[u8]) -> Option<Vec<u8>> {
    let preamble = crate::journal::MAGIC.len() + 2;
    let mut out = bytes.get(..preamble).unwrap_or_default().to_vec();
    let mut rest = bytes.get(preamble..).unwrap_or_default();
    let mut changed = false;
    while let Some((len, after)) = rest.split_first_chunk::<8>() {
        let Some(payload) = usize::try_from(u64::from_be_bytes(*len))
            .ok()
            .and_then(|len| after.get(..len))
        else {
            break;
        };
        let scrubbed = json_frame(payload);
        changed |= scrubbed.is_some();
        let payload_out = scrubbed.unwrap_or_else(|| payload.to_vec());
        out.extend((payload_out.len() as u64).to_be_bytes());
        out.extend(payload_out);
        rest = &after[payload.len()..];
    }
    out.extend(rest);
    changed.then_some(out)
}

/// One journal frame without the key, or `None` when it never held it.
fn json_frame(payload: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(payload).ok()?;
    strip(&mut value).then(|| serde_json::to_vec(&value).unwrap_or_default())
}

/// Take the key out of `value` at every depth. Answers whether it was there.
fn strip(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let here = map.remove(KEY).is_some();
            map.values_mut().fold(here, |found, v| strip(v) | found)
        }
        Value::Array(items) => items.iter_mut().fold(false, |found, v| strip(v) | found),
        _ => false,
    }
}
