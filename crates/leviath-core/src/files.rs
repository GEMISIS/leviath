//! The names of the files a run directory and an agent directory hold.
//!
//! One place for each name. The persistence lane writes these files, the
//! CLI's run state reader, the recovery scan, the dashboard and the HTTP API
//! read them back. A name that lives in one constant cannot be misspelled in
//! one reader and quietly never match again.

/// The run's metadata: status, timings, totals. Written by the persistence
/// lane on every change and read by everything that lists runs.
pub const META_FILE: &str = "meta.json";

/// The run's latest context-window snapshot.
pub const CONTEXT_FILE: &str = "context.json";

/// The per-stage ledger: which stages ran, in what order, at what cost.
pub const STAGES_FILE: &str = "stages.json";

/// The fan-out record for a run that split into workers.
pub const FANOUT_FILE: &str = "fanout.json";

/// The interactions a paused run is waiting on.
pub const INTERACTIONS_FILE: &str = "interactions.json";

/// The run archive: the append-only journal every other file is a view of.
pub const ARCHIVE_FILE: &str = "run.lvr";

/// The blueprint manifest inside an agent directory.
pub const MANIFEST_FILENAME: &str = "agent.leviath";

/// The blueprint a run actually executed, copied into the run directory at
/// spawn.
///
/// A run used to name the installed file it was started from and nothing
/// more, so reading "what did this run execute" meant reading a file that may
/// have been edited or deleted since, and a daemon restart resumed a run on
/// whatever the file said by then. This copy is the run's own: immutable with
/// it, and identified by the digest recorded beside it in `meta.json`.
///
/// The manifest only. Scripts it names (hooks, validators, region scripts) are
/// still read from the installed agent directory, so editing one of those does
/// reach a running run.
pub const BLUEPRINT_SNAPSHOT_FILE: &str = "blueprint.leviath";

/// The directory inside a run holding stored mime parts, one file per
/// SHA-256. Referenced from entries and events by hash, never inlined.
pub const BLOBS_DIR: &str = "blobs";
