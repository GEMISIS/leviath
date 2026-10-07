//! The names of the files a run directory and an agent directory hold.
//!
//! One place for each name. The persistence lane writes the run file, and the
//! CLI's run state reader, the recovery scan, the dashboard and the HTTP API
//! read it back. A name that lives in one constant cannot be misspelled in one
//! reader and quietly never match again.

/// A blueprint's file, inside its directory.
pub const BLUEPRINT_MANIFEST: &str = "agent.toml";

/// The run file: the run's spec, its code and files, the steps it took and
/// checkpoints of its state, in the LVR2 frame format. Everything about a run
/// is here.
pub const RUN_FILE: &str = "run.lvr";

/// The directory inside a run holding stored mime parts, one file per
/// SHA-256. Referenced from entries and events by hash, never inlined.
pub const BLOBS_DIR: &str = "blobs";
