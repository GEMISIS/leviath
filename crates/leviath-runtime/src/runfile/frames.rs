//! The payloads of the frames that are not a spec, a state or a delta.

use serde::{Deserialize, Serialize};

use crate::spec::names::Digest;

/// Code the run uses, stored once by its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeFrame {
    /// The sha256 of `bytes`. First, so a reader can index the frame by
    /// reading only the start of its payload.
    pub digest: Digest,
    /// The code.
    pub bytes: Vec<u8>,
}

/// A stored part's bytes, stored once by their digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobFrame {
    /// The sha256 of `bytes`. First, for the same reason as
    /// [`CodeFrame::digest`].
    pub digest: Digest,
    /// The bytes.
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
