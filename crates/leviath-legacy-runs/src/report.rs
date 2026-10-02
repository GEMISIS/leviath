//! What a conversion did, and every value it had to fill in.

use std::fmt;
use std::path::PathBuf;

use leviath_runtime::spec::names::RunId;

/// A field the old run did not record, and the value the conversion used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaulted {
    /// The field, as a path into the run file (`launch.allow`,
    /// `stages.plan.context_window`).
    pub field: String,
    /// The value it was given.
    pub value: String,
    /// Why the old run could not supply it.
    pub why: String,
}

impl fmt::Display for Defaulted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {} ({})", self.field, self.value, self.why)
    }
}

/// Where the run's graph was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlueprintSource {
    /// The copy the run kept of the blueprint it ran.
    Snapshot,
    /// The installed blueprint at this path, because the run kept no copy.
    /// It may have been edited since the run started.
    Installed(PathBuf),
    /// Neither: the graph is what the run recorded, and the run never
    /// resumes.
    Recorded {
        /// Every place a blueprint was looked for.
        tried: Vec<PathBuf>,
        /// Why none was read.
        why: String,
    },
}

/// What a conversion produced.
#[derive(Debug, Clone)]
pub struct ConvertReport {
    /// The run.
    pub run_id: RunId,
    /// The run file written.
    pub run_file: PathBuf,
    /// Where the old files went.
    pub legacy_dir: PathBuf,
    /// Where the graph came from.
    pub blueprint: BlueprintSource,
    /// How many steps of the old journal became deltas.
    pub deltas: usize,
    /// Every field the conversion had to fill in.
    pub defaulted: Vec<Defaulted>,
    /// Anything else worth knowing about how the run was read.
    pub notes: Vec<String>,
}

impl ConvertReport {
    /// The defaulted entry for `field`, if there is one.
    pub fn defaulted(&self, field: &str) -> Option<&Defaulted> {
        self.defaulted.iter().find(|d| d.field == field)
    }
}

/// Collects the report while the conversion runs.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) defaulted: Vec<Defaulted>,
    pub(crate) notes: Vec<String>,
}

impl Report {
    /// Record that `field` was set to `value` because `why`.
    pub(crate) fn fill(
        &mut self,
        field: impl Into<String>,
        value: impl fmt::Display,
        why: impl Into<String>,
    ) {
        self.defaulted.push(Defaulted {
            field: field.into(),
            value: value.to_string(),
            why: why.into(),
        });
    }

    /// Record a note.
    pub(crate) fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Every line of the report, for the run's log.
    pub(crate) fn log_lines(&self) -> Vec<String> {
        let defaulted = self
            .defaulted
            .iter()
            .map(|d| format!("converted from the old layout: {d}"));
        let notes = self
            .notes
            .iter()
            .map(|n| format!("converted from the old layout: {n}"));
        defaulted.chain(notes).collect()
    }

    pub(crate) fn finish(
        self,
        run_id: RunId,
        blueprint: BlueprintSource,
        deltas: usize,
        (run_file, legacy_dir): (PathBuf, PathBuf),
    ) -> ConvertReport {
        ConvertReport {
            run_id,
            run_file,
            legacy_dir,
            blueprint,
            deltas,
            defaulted: self.defaulted,
            notes: self.notes,
        }
    }
}
