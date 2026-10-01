//! The names of the files a run directory and an agent directory hold.
//!
//! One place for each name. The persistence lane writes the run file, and the
//! CLI's run state reader, the recovery scan, the dashboard and the HTTP API
//! read it back. A name that lives in one constant cannot be misspelled in one
//! reader and quietly never match again.
//!
//! A run directory in the older many-file layout holds `meta.json`,
//! `context.json`, `stages.json`, `fanout.json`, `interactions.json` and an
//! LVR1 journal at `run.lvr`; those names stay here for the readers and the
//! converter that still open such a directory. Nothing writes them for a new
//! run.

/// A run's metadata in the older layout: status, timings, totals.
pub const META_FILE: &str = "meta.json";

/// A run's latest context-window snapshot in the older layout.
pub const CONTEXT_FILE: &str = "context.json";

/// The per-stage ledger in the older layout.
pub const STAGES_FILE: &str = "stages.json";

/// The fan-out record in the older layout.
pub const FANOUT_FILE: &str = "fanout.json";

/// The interactions a paused run waited on, in the older layout.
pub const INTERACTIONS_FILE: &str = "interactions.json";

/// Where the older layout kept its LVR1 journal. The same name as
/// [`RUN_FILE`]: the two are told apart by the magic their first bytes carry.
pub const ARCHIVE_FILE: &str = "run.lvr";

/// The run file: the run's spec, its code and files, the steps it took and
/// checkpoints of its state, in the LVR2 frame format. Everything about a run
/// is here.
pub const RUN_FILE: &str = "run.lvr";

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
