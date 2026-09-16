//! Writing the bundle to a `.zip`.
//!
//! A zip rather than the `.tar.gz` the blueprint bundler writes because the
//! file is for handing to someone: it opens with a double-click on every
//! desktop and a bug tracker accepts it as an attachment. The members are
//! owner-only, and so is the file, since what it holds is private even with
//! every key taken out.
//!
//! The archive is assembled in memory, which is where the bundle already is,
//! and lands on disk in one private write. An in-memory writer cannot fail,
//! so the only error a caller sees is the one about the path they chose.

use std::io::{Cursor, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::collect::Bundle;

/// Write `bundle` to `path`, every member under `root/`, and return the
/// finished file's size in bytes.
pub(crate) fn write_zip(bundle: &Bundle, root: &str, path: &Path) -> anyhow::Result<u64> {
    let bytes = zip_bytes(bundle, root);
    leviath_sys::write_private(path, &bytes)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(bytes.len() as u64)
}

/// The zip as bytes. Member names are unique by construction and the sink
/// is a `Vec`, so nothing here can fail.
pub(crate) fn zip_bytes(bundle: &Bundle, root: &str) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for member in &bundle.members {
        zip.start_file(format!("{root}/{}", member.path), options)
            .expect("a fresh, unique member name");
        zip.write_all(&member.bytes)
            .expect("an in-memory zip accepts writes");
    }
    zip.finish()
        .expect("an in-memory zip finishes")
        .into_inner()
}
