//! What blueprint lint reports.
//!
//! Lint is the set of checks a graph's own validation does not make: the
//! fields whose absence quietly changes what a run does, a tool name that
//! matches nothing, a stage that waits on a person nobody will be there to be.
//! Validation says whether a graph can run; lint says whether it will do what
//! its author meant. These are the findings every lint check reports in, so
//! `lev validate`, the daemon's spawn log and the blueprint editor all show
//! them the same way.

use serde::Serialize;

/// How much a finding matters. Only [`LintSeverity::Error`] fails
/// `lev validate`; warnings are printed and the command still exits zero
/// (unless `--deny-warnings` is passed); notes never fail anything.
///
/// Declared worst-first so sorting by it groups the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LintSeverity {
    /// The blueprint says something that cannot be what the author meant: a
    /// tool name matching nothing, a permission for a tool the stage never
    /// granted.
    Error,
    /// The blueprint leaves a decision to a default the author may not know
    /// about.
    Warning,
    /// Nothing is wrong; the blueprint is doing something worth knowing before
    /// you run it, like reaching outside its workdir or running a shell command
    /// at spawn. A note never fails a build, so `--deny-warnings` skips it.
    Note,
}

impl LintSeverity {
    /// A fixed-width label for a report, so the messages line up.
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERR ",
            Self::Warning => "WARN",
            Self::Note => "NOTE",
        }
    }
}

/// One thing worth telling the author about.
///
/// Serialize only: `code` is a `&'static str` naming a check, which no
/// deserializer can produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintFinding {
    /// How much this matters, and therefore whether it fails the check.
    pub severity: LintSeverity,
    /// A stable slug (`"unknown-tool"`), so a finding can be referenced in an
    /// issue or grepped for in daemon logs without quoting prose.
    pub code: &'static str,
    /// The stage it belongs to, when it belongs to one.
    pub stage: Option<String>,
    /// What is wrong.
    pub message: String,
    /// What to do about it. Rendered on its own indented line.
    pub fix: Option<String>,
}

impl LintFinding {
    /// A finding that belongs to no stage and says nothing about a fix.
    pub fn new(severity: LintSeverity, code: &'static str, message: String) -> Self {
        Self {
            severity,
            code,
            stage: None,
            message,
            fix: None,
        }
    }

    /// This finding, placed in a stage.
    pub fn in_stage(mut self, stage: &str) -> Self {
        self.stage = Some(stage.to_string());
        self
    }

    /// This finding, with what to do about it.
    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// Whether this finding fails the check.
    pub fn is_error(&self) -> bool {
        self.severity == LintSeverity::Error
    }

    /// One line for a log record: `stage 'x': message`.
    pub fn one_line(&self) -> String {
        match &self.stage {
            Some(stage) => format!("stage '{stage}': {}", self.message),
            None => self.message.clone(),
        }
    }
}
