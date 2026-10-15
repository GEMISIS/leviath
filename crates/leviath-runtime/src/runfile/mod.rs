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
pub use events::{Answered, journal_events, journal_events_with, outcome_of};
pub use reader::{RunFileReader, blob_path, read_blob};
pub use summary::{as_it_stands, context_snapshot, stage_records, summary, summary_of};
pub use tail::{RunFileTail, read_spec};
pub use writer::{CheckpointPolicy, RunFileWriter};

/// The JSON Schemas of every type a run file stores, as one document.
///
/// `cargo xtask schema` writes it to `docs/schema/run-file.schema.json` (a
/// test holds the file to it). Another test holds the shape it describes
/// still, leaving out what only documents it, so a frame type cannot change
/// shape without the layout's [`FINGERPRINT`] being looked at.
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
/// binary shape. So a test also encodes a fully populated sample of every
/// frame and compares its bytes with a recorded hash: a change to the binary
/// layout fails that test until this number is bumped and
/// [`FINGERPRINT`] given a new value.
pub const LAYOUT_VERSION: u32 = 4;

/// The name of [`LAYOUT_VERSION`]'s binary layout, in every run file's
/// header: two builds share it exactly when their run files have the same
/// shape.
///
/// It changes only with the layout. Rewording a frame type's documentation
/// changes neither; a test holds each frame type's structure, and its
/// failure says when this needs a new value and an upgrade for the files
/// carrying the old one.
pub const FINGERPRINT: [u8; 32] = [
    0x02, 0xc9, 0xe6, 0x0f, 0x25, 0xc0, 0xe2, 0x97, 0xb4, 0xde, 0xcd, 0x90, 0x28, 0x2b, 0x25, 0x63,
    0x74, 0x91, 0xa2, 0xad, 0x60, 0xa5, 0xb4, 0x64, 0x8f, 0x9e, 0x13, 0x89, 0x75, 0xed, 0xf5, 0xe2,
];

/// [`FINGERPRINT`], the header every run file this build writes carries and
/// every one it reads must.
pub fn fingerprint() -> &'static [u8; 32] {
    &FINGERPRINT
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
