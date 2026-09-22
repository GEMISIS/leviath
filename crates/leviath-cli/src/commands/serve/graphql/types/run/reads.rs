//! Where a run's own file reads are reported.
//!
//! The lazy walk's whole promise is about reads that do not happen: page two
//! opens nothing for the runs page one already passed, and a filter answerable
//! from memory opens nothing at all. A promise about reads is worth what a test
//! can check, so the fields that read report here and a test counts them.
//!
//! Nothing installs a recorder in a server. The list would grow for ever and
//! nothing but a test has any use for it, so a server's reads cost a load of
//! [`RECORDER`] and a call it does not make.

/// What a run's file read is reported to.
pub(crate) type FileReadRecorder = Box<dyn Fn(&str) + Send + Sync>;

/// Where a run's file read is reported, when anything is listening.
static RECORDER: std::sync::OnceLock<FileReadRecorder> = std::sync::OnceLock::new();

/// Report every file read to `record`, for as long as this process lives.
///
/// Once, deliberately: the recorder is consulted on a hot path, and there is
/// nothing to gain from letting it change under one. A second call is a no-op,
/// so each test that wants the log can ask for it rather than arranging to be
/// the one that installs it.
#[cfg(test)]
pub(crate) fn record_file_reads(record: FileReadRecorder) {
    drop(RECORDER.set(record));
}

/// Note that a run is about to open one of its own files.
pub(crate) fn counted(run_id: &str) {
    if let Some(record) = RECORDER.get() {
        record(run_id);
    }
}
