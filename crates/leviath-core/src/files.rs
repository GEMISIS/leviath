//! The names of the files a run directory and an agent directory hold.
//!
//! One place for each name. The persistence lane writes the run file, and the
//! CLI's run state reader, the recovery scan, the dashboard and the HTTP API
//! read it back. A name that lives in one constant cannot be misspelled in one
//! reader and quietly never match again.
//!
//! The older many-file layout named its files `meta.json`, `context.json`,
//! `stages.json`, `fanout.json`, `interactions.json` and an LVR1 journal at
//! `run.lvr`. Those names stay here for the converter that reads such a
//! directory and the recovery scan that finds one. Nothing writes them.

/// What the older layout named a run's metadata (status, timings, totals),
/// which the converter reads. `lev rage` also files a run's record under this
/// name in its bundle.
pub const META_FILE: &str = "meta.json";

/// What the older layout named a run's latest context-window snapshot, which
/// the converter reads.
pub const CONTEXT_FILE: &str = "context.json";

/// What the older layout named the per-stage ledger, which the converter
/// reads.
pub const STAGES_FILE: &str = "stages.json";

/// What the older layout named the fan-out record, which the converter reads.
pub const FANOUT_FILE: &str = "fanout.json";

/// What the older layout named the interactions a paused run waited on, which
/// the converter reads.
pub const INTERACTIONS_FILE: &str = "interactions.json";

/// What the older layout named its LVR1 journal, which the converter reads.
/// The same name as
/// [`RUN_FILE`]: the two are told apart by the magic their first bytes carry.
pub const ARCHIVE_FILE: &str = "run.lvr";

/// A blueprint's file, inside its directory.
pub const BLUEPRINT_MANIFEST: &str = "agent.toml";

/// The run file: the run's spec, its code and files, the steps it took and
/// checkpoints of its state, in the LVR2 frame format. Everything about a run
/// is here.
pub const RUN_FILE: &str = "run.lvr";

/// What the older layout named the copy of the blueprint a run executed,
/// which the converter reads. The run file holds what a run executed.
pub const BLUEPRINT_SNAPSHOT_FILE: &str = "blueprint.leviath";

/// The directory inside a run holding stored mime parts, one file per
/// SHA-256. Referenced from entries and events by hash, never inlined.
pub const BLOBS_DIR: &str = "blobs";
