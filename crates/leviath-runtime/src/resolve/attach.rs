//! Step 3: the files sent with a request.
//!
//! Each attachment is checked on its own: its name is unique, it is within
//! the operator's size limit, and its type is known. One that fails is left
//! out, and the rest carry on. Where each one lands is decided later, once the
//! inputs say which of them a `file` input takes.

use std::collections::{BTreeMap, BTreeSet};

use leviath_core::mime::{Blob, MimeRegistry, MimeType};

use crate::spec::env::{ResolveEnv, SpawnLimits};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{Digest, MimePattern};
use crate::spec::request::SpawnRequest;
use crate::state::context::{BlobState, PartBody, PartState};

/// An attachment that passed its checks.
#[derive(Debug, Clone)]
pub(super) struct File {
    /// Its position in the request, for issue paths.
    pub(super) index: usize,
    /// Its name.
    pub(super) name: String,
    /// Its type, as the machine read it.
    pub(super) mime: MimeType,
    /// The part it becomes in a region.
    pub(super) part: PartState,
    /// Its bytes.
    pub(super) bytes: Vec<u8>,
}

/// Every attachment that passed its checks, in request order.
#[derive(Debug, Clone, Default)]
pub(super) struct Files(pub(super) Vec<File>);

impl Files {
    /// An attachment by name.
    pub(super) fn get(&self, name: &str) -> Option<&File> {
        self.0.iter().find(|f| f.name == name)
    }

    /// The bytes of every attachment, by digest.
    pub(super) fn blobs(&self) -> BTreeMap<Digest, Vec<u8>> {
        self.0
            .iter()
            .map(|f| (Digest::of(&f.bytes), f.bytes.clone()))
            .collect()
    }
}

/// Check every attachment on the request.
///
/// Stand-ins and token estimates come from the compiled mime registry: the
/// operator's own rows are not something resolution can see, and the part is
/// charged its stand-in in a region either way.
pub(super) fn read(
    request: &SpawnRequest,
    env: &dyn ResolveEnv,
    limits: &SpawnLimits,
    issues: &mut SpawnIssues,
) -> Files {
    let registry = MimeRegistry::builtin();
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for (index, attachment) in request.attachments.iter().enumerate() {
        let at = SpecPath::root().field("attachments").index(index);
        if !seen.insert(attachment.name.as_str()) {
            issues.push(
                SpawnIssue::new(
                    at.field("name"),
                    IssueCode::Duplicate,
                    format!("two attachments are named \"{}\"", attachment.name),
                )
                .hint("give each attached file its own name"),
            );
            continue;
        }
        let bytes = &attachment.data.0;
        let size = bytes.len() as u64;
        if size > limits.max_attachment_bytes {
            issues.push(
                SpawnIssue::new(
                    at.field("data"),
                    IssueCode::OutOfRange,
                    "the file is larger than this machine accepts",
                )
                .expected(format!("at most {} bytes", limits.max_attachment_bytes))
                .got(format!("{size} bytes")),
            );
            continue;
        }
        let typed = env
            .sniff(&attachment.name, bytes, attachment.mime_type.as_ref())
            .and_then(|t| MimeType::parse(&t).map_err(|e| e.to_string()));
        let mime = match typed {
            Ok(mime) => mime,
            Err(message) => {
                let declared = attachment.mime_type.as_ref().map(MimePattern::as_str);
                issues.push(
                    SpawnIssue::new(at.field("mime_type"), IssueCode::Invalid, message)
                        .got(declared.unwrap_or("no declared type").to_string())
                        .hint("declare the file's mime type, or send bytes of the type it names"),
                );
                continue;
            }
        };
        let blob = Blob::new(mime.clone(), bytes.clone())
            .named(attachment.name.clone())
            .describe(&registry);
        let part = PartState {
            mime_type: mime.to_string(),
            body: PartBody::Stored(BlobState {
                digest: Digest::of(bytes),
                size,
                width: blob.width,
                height: blob.height,
                duration_ms: blob.duration_ms,
                tokens: u32::try_from(blob.tokens).unwrap_or(u32::MAX),
                stand_in: blob.stand_in,
            }),
            name: Some(attachment.name.clone()),
            deliver: attachment.deliver,
        };
        files.push(File {
            index,
            name: attachment.name.clone(),
            mime,
            part,
            bytes: bytes.clone(),
        });
    }
    Files(files)
}

/// Whether a region or input that accepts `patterns` takes a file of type
/// `mime`. An empty list takes anything.
pub(super) fn accepts(mime: &MimeType, patterns: &[MimePattern]) -> bool {
    patterns.is_empty() || mime.matches_any(patterns)
}
