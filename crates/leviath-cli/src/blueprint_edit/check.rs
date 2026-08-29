//! What is wrong with a manifest, the way `lev validate` and the daemon's
//! `POST /api/blueprints/validate` would say it: parse, then
//! `Blueprint::validate`, then the lint, with lint errors blocking a save
//! and warnings and notes shown.

use std::path::Path;

use leviath_core::ValidationError;
use leviath_core::manifest::parse_manifest;

use crate::lint::{LintEnv, LintSeverity, lint_manifest};

/// How much a problem matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    /// The manifest will not run, or will not save.
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
    /// stages before the lint.
    pub code: &'static str,
    /// The stage it belongs to, when the message names one.
    pub stage: Option<String>,
    /// What is wrong.
    pub message: String,
    /// What to do about it, when the lint knows.
    pub fix: Option<String>,
}

/// Everything wrong with a manifest, most serious first.
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

    /// Whether the manifest may be saved: no errors.
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

/// Check a manifest as the runtime would. `dir` is the blueprint's directory
/// (for the tools its scripts define); a blueprint not yet saved anywhere
/// can pass any directory.
pub(crate) fn check(text: &str, dir: &Path) -> Problems {
    let bp = match parse_manifest(text) {
        Ok(bp) => bp,
        Err(e) => {
            return Problems {
                items: vec![Problem {
                    severity: Severity::Error,
                    code: "parse",
                    stage: None,
                    message: e.to_string(),
                    fix: None,
                }],
            };
        }
    };
    if let Err(e) = bp.validate() {
        let stage = match &e {
            ValidationError::Stage { stage, .. }
            | ValidationError::Transition { from: stage, .. } => Some(stage.clone()),
            _ => None,
        };
        return Problems {
            items: vec![Problem {
                severity: Severity::Error,
                code: "validate",
                stage,
                message: e.to_string(),
                fix: None,
            }],
        };
    }
    let env = LintEnv::offline(dir);
    let mut items: Vec<Problem> = lint_manifest(text, &bp, &env)
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
