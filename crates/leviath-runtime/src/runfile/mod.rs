//! The run file: one file per run holding everything about it.
//!
//! A run file starts with the run's [`RunSpec`](crate::spec::run_spec::RunSpec),
//! the state it started from. The code and blobs it uses follow, stored once
//! each by digest. Then come [`StateDelta`](crate::state::StateDelta)s, one
//! per step, with a full [`RunState`](crate::state::RunState) checkpoint
//! every so often. The last checkpoint plus the deltas after it is where the
//! run is now; any earlier point is the spec's state with the deltas up to
//! it applied.
//!
//! [`codec`] is the byte layout, [`writer`] and [`reader`] write and read
//! it, and [`view`] renders what it holds as TOML.

use std::sync::OnceLock;

use sha2::Digest as _;

pub mod codec;
mod error;
pub mod frames;
pub(crate) mod lane;
pub mod reader;
pub mod view;
pub mod writer;

pub use error::{RunFileError, RunFileErrorKind};
pub use reader::RunFileReader;
pub use writer::{CheckpointPolicy, RunFileWriter};

/// The JSON Schemas of every type a run file stores, as one document.
///
/// `cargo xtask schema` writes it to `docs/schema/run-file.schema.json`, and
/// its hash is the fingerprint every run file's header carries.
pub fn frame_schemas() -> serde_json::Value {
    serde_json::json!({
        "spec": schemars::schema_for!(crate::spec::run_spec::RunSpec),
        "state": schemars::schema_for!(crate::state::RunState),
        "delta": schemars::schema_for!(crate::state::StateDelta),
        "code": schemars::schema_for!(frames::CodeFrame),
        "blob": schemars::schema_for!(frames::BlobFrame),
        "owner": schemars::schema_for!(frames::OwnerFrame),
    })
}

/// The hash of [`frame_schemas`]: two builds share it exactly when their run
/// files have the same shape.
pub fn fingerprint() -> &'static [u8; 32] {
    static FP: OnceLock<[u8; 32]> = OnceLock::new();
    FP.get_or_init(|| sha2::Sha256::digest(frame_schemas().to_string().as_bytes()).into())
}

/// The JSON Schema of a spawn request, as published for outside tools and
/// agents.
pub fn spawn_request_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(crate::spec::request::SpawnRequest))
        .expect("a schema is plain JSON")
}

#[cfg(test)]
#[path = "reader_tests.rs"]
pub(crate) mod reader_tests;

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
