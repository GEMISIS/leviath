//! What is wrong with an `agent.toml`, the way `lev validate` would say it:
//! read the file, check that its graph holds together, then lint it, with
//! lint errors blocking a save and warnings and notes shown.

use std::path::Path;

use leviath_blueprint::BlueprintFile;
use leviath_runtime::spec::issues::{PathSeg, SpawnIssue, SpecPath};

use crate::lint::{LintEnv, LintSeverity, lint_blueprint};

/// How much a problem matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    /// The blueprint will not run, or will not save.
    Error,
    /// A decision left to a default the author may not know about.
    Warning,
    /// Worth knowing, nothing to fix.
    Note,
}

impl Severity {
    /// A short tag for a status line.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One thing to tell the author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Problem {
    /// How much it matters.
    pub severity: Severity,
    /// A stable slug: a lint code, or `parse` / `validate` for the two
    /// passes before the lint.
    pub code: &'static str,
    /// The stage it belongs to, when the message names one.
    pub stage: Option<String>,
    /// What is wrong.
    pub message: String,
    /// What to do about it, when the lint knows.
    pub fix: Option<String>,
}

/// Everything wrong with a blueprint, most serious first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Problems {
    /// In the order found: errors, then warnings, then notes.
    pub items: Vec<Problem>,
}

impl Problems {
    /// How many are errors.
    pub(crate) fn error_count(&self) -> usize {
        self.count(Severity::Error)
    }

    /// How many are warnings.
    pub(crate) fn warning_count(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, severity: Severity) -> usize {
        self.items.iter().filter(|p| p.severity == severity).count()
    }

    /// Whether the blueprint may be saved: no errors.
    pub(crate) fn is_saveable(&self) -> bool {
        self.error_count() == 0
    }

    /// The problems that name `stage`.
    pub(crate) fn for_stage(&self, stage: &str) -> Vec<&Problem> {
        self.items
            .iter()
            .filter(|p| p.stage.as_deref() == Some(stage))
            .collect()
    }

    /// The most serious problem, for a one-line summary.
    pub(crate) fn first(&self) -> Option<&Problem> {
        self.items.first()
    }
}

/// Check an `agent.toml` as the runtime would. `dir` is the blueprint's
/// directory (for the tools its scripts define); a blueprint not yet saved
/// anywhere can pass any directory.
pub(crate) fn check(text: &str, dir: &Path) -> Problems {
    let file = match BlueprintFile::parse(text) {
        Ok(file) => file,
        Err(message) => {
            return Problems {
                items: vec![Problem {
                    severity: Severity::Error,
                    code: "parse",
                    stage: None,
                    message,
                    fix: None,
                }],
            };
        }
    };
    if let Err(issues) = file.graph.validate(&SpecPath::root().field("graph")) {
        let items = issues
            .iter()
            .map(|issue| Problem {
                severity: Severity::Error,
                code: "validate",
                stage: stage_of(&file, issue),
                message: issue.to_string(),
                fix: None,
            })
            .collect();
        return Problems { items };
    }
    let env = LintEnv::offline(dir);
    let mut items: Vec<Problem> = lint_blueprint(&file, &env)
        .into_iter()
        .map(|f| Problem {
            severity: match f.severity {
                LintSeverity::Error => Severity::Error,
                LintSeverity::Warning => Severity::Warning,
                LintSeverity::Note => Severity::Note,
            },
            code: f.code,
            stage: f.stage,
            message: f.message,
            fix: f.fix,
        })
        .collect();
    // Errors first, then warnings, then notes; the lint's own order within
    // each, since it walks the stages in order.
    items.sort_by_key(|p| match p.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Note => 2,
    });
    Problems { items }
}

/// The stage an issue is about: the stage at `graph.stages.<name>` (or
/// `graph.stages[i]`), or the stage the edge at `graph.edges[i]` leaves.
fn stage_of(file: &BlueprintFile, issue: &SpawnIssue) -> Option<String> {
    let [_, PathSeg::Field(list), at, ..] = issue.path.0.as_slice() else {
        return None;
    };
    let graph = &file.graph;
    match (list.as_str(), at) {
        ("stages", PathSeg::Key(name)) => Some(name.clone()),
        ("stages", PathSeg::Index(i)) => graph.stages.get(*i).map(|s| s.name.to_string()),
        ("edges", PathSeg::Index(i)) => graph.edges.get(*i).map(|e| e.from.to_string()),
        _ => None,
    }
}
