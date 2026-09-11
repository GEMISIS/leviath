//! The files a submission names, checked against what the stage declared.
//!
//! An artifact is named by path (the old shape) or by `{ name, path, type }`.
//! Every path must resolve inside the workdir and name a file that exists;
//! every declared `required` artifact must be present; an artifact that a
//! declaration names must be of the declared type. What survives is typed
//! by the registry (a declared type wins), hashed, and stored as a part of
//! the run when it fits the size ceiling, so a later stage and the API see
//! the file the way they see any other part.

use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part, sha256_hex};
use leviath_core::output::{Artifact, ArtifactSpec};

use crate::context_setup::PartSink;

/// What resolving a submission's artifacts produced.
#[derive(Debug)]
pub(super) struct Ingested {
    /// The records the answer carries.
    pub records: Vec<Artifact>,
    /// The stored parts, for the `final_output` region. One per record the
    /// store took; a file over the ceiling has a record and no part.
    pub parts: Vec<Part>,
}

/// One named entry of the `artifacts` argument, before it is checked.
struct Named {
    name: Option<String>,
    path: String,
    declared: Option<MimeType>,
}

/// Read the `artifacts` argument in either shape.
fn listed(args: &serde_json::Value) -> Result<Vec<Named>, String> {
    let Some(items) = args.get("artifacts").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in items {
        match item {
            serde_json::Value::String(path) if !path.trim().is_empty() => out.push(Named {
                name: None,
                path: path.clone(),
                declared: None,
            }),
            serde_json::Value::String(_) => {}
            serde_json::Value::Object(map) => {
                let path = map
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| {
                        "[error] an artifact given as an object needs a path".to_string()
                    })?;
                let declared = match map.get("type").and_then(|v| v.as_str()) {
                    Some(t) => Some(MimeType::parse(t).map_err(|e| {
                        format!("[error] artifact '{path}' has type '{t}', which is not one: {e}")
                    })?),
                    None => None,
                };
                out.push(Named {
                    name: map
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(str::to_string),
                    path: path.to_string(),
                    declared,
                });
            }
            other => {
                return Err(format!(
                    "[error] each artifact must be a path or an object with name, path and \
                     type, not {other}"
                ));
            }
        }
    }
    Ok(out)
}

/// Resolve `name_or_sha` against the parts the run has already produced and,
/// when it names one, write that part's bytes to `dest` so the submission has a
/// real file to record. Matches the part's name (exact, then its file name),
/// then a sha256 prefix. Returns the bytes written, or the same shape of error
/// the caller gives for a missing workdir file.
fn materialize_produced(
    name_or_sha: &str,
    dest: &std::path::Path,
    produced: &[Part],
    sink: Option<&PartSink<'_>>,
) -> Result<Vec<u8>, String> {
    let missing = || {
        format!(
            "[error] artifact '{name_or_sha}' is neither a file in the working directory nor a \
             part this run produced. Write the file before naming it, or name a produced part."
        )
    };
    let basename = std::path::Path::new(name_or_sha)
        .file_name()
        .map(|n| n.to_string_lossy().to_string());
    let is_match = |p: &&Part| {
        let by_name = p.name.as_deref() == Some(name_or_sha)
            || (basename.is_some() && p.name.as_deref() == basename.as_deref());
        let by_sha =
            name_or_sha.len() >= 8 && p.blob().is_some_and(|b| b.sha256.starts_with(name_or_sha));
        by_name || by_sha
    };
    let blob = produced.iter().find(is_match).and_then(Part::blob);
    let (Some(blob), Some(sink)) = (blob, sink) else {
        return Err(missing());
    };
    let bytes = sink.store.read(sink.run_id, &blob.sha256).map_err(|e| {
        format!("[error] artifact '{name_or_sha}' could not be read from the run's store: {e}")
    })?;
    // Written to the path as given, whose directory must exist - the same as a
    // regular artifact, which is a file already on disk. A name with a missing
    // parent fails here with the reason, rather than silently making the tree.
    std::fs::write(dest, bytes.as_ref())
        .map_err(|e| format!("[error] artifact '{name_or_sha}' could not be written: {e}"))?;
    Ok(bytes.to_vec())
}

/// Check, type, hash and store the artifacts a submission names.
pub(super) fn resolve(
    args: &serde_json::Value,
    workdir: Option<&std::path::Path>,
    declared: &[ArtifactSpec],
    produced: &[Part],
    sink: Option<&PartSink<'_>>,
) -> Result<Ingested, String> {
    let named = listed(args)?;
    if named.is_empty() {
        let missing: Vec<&str> = declared
            .iter()
            .filter(|d| d.required)
            .map(|d| d.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "[error] this stage must submit these artifacts: {}. Name each one in \
                 `artifacts` as {{ name, path }}.",
                missing.join(", ")
            ));
        }
        return Ok(Ingested {
            records: Vec::new(),
            parts: Vec::new(),
        });
    }
    // No workdir means nothing to resolve against, so nothing can be verified.
    // Unreachable for a real run (every one carries its metadata); loud rather
    // than silent if it ever is.
    let Some(workdir) = workdir else {
        return Err(
            "[error] cannot record artifacts: this run has no working directory to resolve \
             them against"
                .to_string(),
        );
    };
    let builtin;
    let registry: &MimeRegistry = match sink {
        Some(s) => s.registry,
        None => {
            builtin = MimeRegistry::builtin();
            &builtin
        }
    };
    let mut records = Vec::new();
    let mut parts = Vec::new();
    for item in named {
        let full = workdir.join(&item.path);
        if !leviath_core::resolves_within(&full, workdir) {
            return Err(format!(
                "[error] artifact path '{}' does not resolve inside the working directory",
                item.path
            ));
        }
        let bytes = match std::fs::read(&full) {
            Ok(b) => b,
            // Not a file in the workdir. It may still be a part the run
            // produced (an image a model drew) that never touched disk: name,
            // then sha, resolves it, and it is written to the named path so
            // the user and any later stage get a real file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                materialize_produced(&item.path, &full, produced, sink)?
            }
            Err(e) => {
                return Err(format!(
                    "[error] artifact '{}' could not be read: {e}. Write the file before \
                     naming it.",
                    item.path
                ));
            }
        };
        // A path that read as a file has a final component.
        let file_name = full
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = item.name.unwrap_or_else(|| file_name.clone());
        let mime_type = registry.resolve(item.declared.as_ref(), Some(&file_name), &bytes);
        if let Some(spec) = declared.iter().find(|d| d.name == name)
            && !mime_type.matches(&spec.mime_type)
        {
            return Err(format!(
                "[error] artifact '{name}' must be {}, and '{}' is {mime_type}",
                spec.mime_type, item.path
            ));
        }
        let size = bytes.len() as u64;
        let sha256 = sha256_hex(&bytes);
        if let Some(sink) = sink
            && size <= sink.max_part_bytes
        {
            let blob = Blob::new(mime_type.clone(), bytes).named(&name);
            match sink.store.put(sink.run_id, &blob, sink.registry) {
                Ok(reference) => parts.push(Part::stored(reference).named(&name)),
                Err(e) => tracing::warn!(artifact = %name, "[mime] artifact not stored: {e}"),
            }
        }
        records.retain(|r: &Artifact| r.name != name);
        records.push(Artifact {
            name,
            path: item.path,
            mime_type,
            size,
            sha256,
        });
    }
    let missing: Vec<&str> = declared
        .iter()
        .filter(|d| d.required && !records.iter().any(|r| r.name == d.name))
        .map(|d| d.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "[error] the submission is missing these required artifacts: {}",
            missing.join(", ")
        ));
    }
    Ok(Ingested { records, parts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::{BlobStore, MemoryBlobStore};
    use serde_json::json;

    fn specs() -> Vec<ArtifactSpec> {
        vec![
            ArtifactSpec {
                name: "final".to_string(),
                mime_type: "video/*".to_string(),
                required: true,
                description: None,
            },
            ArtifactSpec {
                name: "notes".to_string(),
                mime_type: "text/*".to_string(),
                required: false,
                description: Some("the shot list".to_string()),
            },
        ]
    }

    #[test]
    fn both_shapes_are_read_and_checked_against_the_declaration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cut.mp4"), b"\x00\x00\x00\x18ftypmp42").unwrap();
        std::fs::write(dir.path().join("notes.md"), "# shots").unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0; 64]).unwrap();
        let store = MemoryBlobStore::new();
        let registry = MimeRegistry::builtin();
        let sink = PartSink {
            store: &store,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 32,
        };
        let args = json!({"artifacts": [
            {"name": "final", "path": "cut.mp4", "type": "video/mp4"},
            "notes.md",
            {"name": "notes", "path": "notes.md"},
            "big.bin",
            "",
        ]});
        let out = resolve(&args, Some(dir.path()), &specs(), &[], Some(&sink)).unwrap();
        let names: Vec<&str> = out.records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["final", "notes.md", "notes", "big.bin"]);
        assert_eq!(out.records[0].mime_type.as_str(), "video/mp4");
        assert_eq!(out.records[0].size, 12);
        assert_eq!(out.records[0].sha256.len(), 64);
        assert_eq!(out.records[2].mime_type.as_str(), "text/markdown");
        assert_eq!(
            out.records[3].mime_type.as_str(),
            "application/octet-stream"
        );
        assert_eq!(
            out.parts.len(),
            3,
            "the 64-byte file is over the 32-byte ceiling"
        );
        assert!(store.has("run-1", &out.records[0].sha256));
        // Naming the same artifact twice keeps the last.
        let args = json!({"artifacts": [
            {"name": "final", "path": "cut.mp4"}, {"name": "final", "path": "cut.mp4"}
        ]});
        let out = resolve(&args, Some(dir.path()), &specs(), &[], Some(&sink)).unwrap();
        assert_eq!(out.records.len(), 1);
        // Without a sink: typed by the built-in registry, nothing stored.
        let out = resolve(&args, Some(dir.path()), &[], &[], None).unwrap();
        assert_eq!(out.records[0].mime_type.as_str(), "video/mp4");
        assert!(out.parts.is_empty());
    }

    #[test]
    fn every_refusal_names_the_artifact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "# shots").unwrap();
        // A directory reads with an error that is not NotFound, so it does not
        // fall through to the produced-part resolution: it is a plain read
        // failure with the original wording.
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        let cases = [
            (json!({}), "must submit these artifacts: final"),
            (json!({"artifacts": ["adir"]}), "could not be read"),
            (
                json!({"artifacts": ["notes.md"]}),
                "missing these required artifacts: final",
            ),
            (
                json!({"artifacts": [{"name": "final", "path": "notes.md"}]}),
                "artifact 'final' must be video/*, and 'notes.md' is text/markdown",
            ),
            (json!({"artifacts": [{"name": "x"}]}), "needs a path"),
            (
                json!({"artifacts": [{"path": "notes.md", "type": "nope"}]}),
                "not one",
            ),
            (json!({"artifacts": [5]}), "each artifact must be"),
            (
                json!({"artifacts": ["../etc/passwd"]}),
                "does not resolve inside",
            ),
            (json!({"artifacts": ["missing.mp4"]}), "neither a file"),
        ];
        for (args, expect) in cases {
            let err = resolve(&args, Some(dir.path()), &specs(), &[], None).unwrap_err();
            assert!(err.contains(expect), "{args}: {err}");
        }
        let err = resolve(&json!({"artifacts": ["notes.md"]}), None, &[], &[], None).unwrap_err();
        assert!(err.contains("working directory"), "{err}");
        // Nothing declared, nothing named: nothing to check.
        assert!(
            resolve(&json!({}), None, &[], &[], None)
                .unwrap()
                .records
                .is_empty()
        );
        // A store that refuses keeps the record and drops the part.
        struct Broken;
        impl BlobStore for Broken {
            fn put(
                &self,
                _: &str,
                _: &Blob,
                _: &MimeRegistry,
            ) -> std::io::Result<leviath_core::mime::BlobRef> {
                Err(std::io::Error::other("full"))
            }
            fn read(&self, _: &str, _: &str) -> std::io::Result<std::sync::Arc<[u8]>> {
                Err(std::io::Error::other("no"))
            }
            fn copy(&self, _: &str, _: &str, _: &str) -> std::io::Result<()> {
                Err(std::io::Error::other("no"))
            }
            fn list(&self, _: &str) -> std::io::Result<Vec<String>> {
                Ok(Vec::new())
            }
        }
        assert!(Broken.read("r", "s").is_err());
        assert!(Broken.copy("r", "s", "t").is_err());
        assert!(Broken.list("r").unwrap().is_empty());
        let registry = MimeRegistry::builtin();
        let sink = PartSink {
            store: &Broken,
            registry: &registry,
            run_id: "run-1",
            max_part_bytes: 1024,
        };
        let out = resolve(
            &json!({"artifacts": ["notes.md"]}),
            Some(dir.path()),
            &[],
            &[],
            Some(&sink),
        )
        .unwrap();
        assert_eq!(out.records.len(), 1);
        assert!(out.parts.is_empty());
    }
}
