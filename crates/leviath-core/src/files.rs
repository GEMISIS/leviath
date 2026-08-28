//! The names of the files a run directory and an agent directory hold.
//!
//! One place for each name. The persistence lane writes these files, the
//! CLI's run state reader, the recovery scan, the dashboard and the HTTP API
//! read them back, and each of them used to spell the name out where it was
//! used. A name that lives in one constant cannot be misspelled in one reader
//! and quietly never match again.

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
