//! The run file: one file per run holding everything about it.
//!
//! A run file starts with the run's [`RunSpec`](crate::spec::run_spec::RunSpec),
//! the state it started from. The code it uses follows, stored once by
//! digest. Then come [`StateDelta`](crate::state::StateDelta)s, one
//! per step, with a full [`RunState`](crate::state::RunState) checkpoint
//! every so often. The last checkpoint plus the deltas after it is where the
//! run is now; any earlier point is the spec's state with the deltas up to
//! it applied. The run's answer, logs, audits and stored parts are files
//! beside it that its state names (see [`crate::state::files`]).
//!
//! [`codec`] is the byte layout, [`writer`] and [`reader`] write and read
//! it, [`tail`] reads only where a run is now, and [`view`] renders what it holds as TOML. [`record`] is what is
//! sent to the world as things happen, which the world folds into the events
//! of each step, and [`history`] is what a reader makes of the steps.

use std::sync::OnceLock;

use sha2::Digest as _;

pub mod codec;
mod error;
pub(crate) mod events;
pub mod frames;
pub mod history;
pub(crate) mod lane;
pub mod reader;
pub mod record;
mod recorded;
mod summary;
pub mod tail;
pub mod view;
pub mod writer;

pub use error::{RunFileError, RunFileErrorKind};
pub use events::{Answered, journal_events, journal_events_with};
pub use reader::{RunFileReader, blob_path, read_blob};
pub use summary::{context_snapshot, stage_records, summary, summary_of};
pub use tail::{RunFileTail, read_spec};
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
        "owner": schemars::schema_for!(frames::OwnerFrame),
    })
}

/// The binary layout's version.
///
/// [`frame_schemas`] describes the readable form of every frame type, and a
/// few graph types read and write a short form there that hides their
/// binary shape. So the fingerprint also carries this number, and a test
/// encodes a fully populated sample of every frame and compares its bytes
/// with a recorded hash: a change to the binary layout fails that test until
/// this number is bumped.
pub const LAYOUT_VERSION: u32 = 2;

/// The hash of [`frame_schemas`] and [`LAYOUT_VERSION`]: two builds share it
/// exactly when their run files have the same shape.
pub fn fingerprint() -> &'static [u8; 32] {
    static FP: OnceLock<[u8; 32]> = OnceLock::new();
    FP.get_or_init(|| {
        let mut h = sha2::Sha256::new();
        h.update(frame_schemas().to_string().as_bytes());
        h.update(LAYOUT_VERSION.to_le_bytes());
        h.finalize().into()
    })
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
