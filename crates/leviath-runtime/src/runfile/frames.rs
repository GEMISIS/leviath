//! The payloads of the frames that are not a spec, a state or a delta.

use serde::{Deserialize, Serialize};

use crate::spec::names::Digest;

/// Code the run uses, stored once by its digest.
///
/// Code is the one kind of bytes the file holds itself: the spec names the
/// scripts the run was resolved against, and a resume binds exactly those,
/// whatever has become of the files they were read from. Everything else a
/// run keeps beside its file and names (see [`crate::state::files`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeFrame {
    /// The sha256 of `bytes`. First, so a reader can index the frame by
    /// reading only the start of its payload.
    pub digest: Digest,
    /// The code.
    pub bytes: Vec<u8>,
}

/// A machine and daemon taking the run over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OwnerFrame {
    /// The machine's stable id.
    pub machine_id: String,
    /// The daemon process's id.
    pub world_id: String,
    /// When, in unix seconds.
    pub at: i64,
}
