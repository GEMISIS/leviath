//! What a scoped approval of a tool call would record, as a person and a
//! client read it.
//!
//! A grant is a set of keys (see `leviath_cli::shell_keys` for how a shell
//! line is keyed). A key is an opaque string to whatever matches it; this is
//! the one place that reads a key back into what it names, so the option
//! label and the structured [`ApprovalGrant`] a client receives say the same
//! thing.

use serde::{Deserialize, Serialize};

use super::GrantKeys;

/// The namespace every shell key carries.
const SHELL_PREFIX: &str = "shell:";

/// The namespace of a key naming one path outside the working directory that
/// one program was handed.
///
/// Its own namespace rather than a spelling inside `shell:`, so no program
/// name, however it is quoted, can produce one.
const OUTSIDE_PATH_PREFIX: &str = "outside:";

/// The key naming `path`, outside the working directory, as an argument of
/// `program`.
///
/// The pair is written as a JSON array, so two different pairs never share a
/// key whatever characters the program or the path holds.
pub fn outside_path_key(program: &str, path: &str) -> String {
    format!(
        "{OUTSIDE_PATH_PREFIX}{}",
        serde_json::json!([program, path])
    )
}

/// The program and the path an outside-path key names, or `None` when `key`
/// is not one.
pub fn outside_path_of(key: &str) -> Option<(String, String)> {
    serde_json::from_str(key.strip_prefix(OUTSIDE_PATH_PREFIX)?).ok()
}

/// What kind of thing one grant key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantKeyKind {
    /// A tool, by name: every call of it.
    Tool,
    /// A program a shell line runs, with the subcommand that narrows it when
    /// it has one.
    Program,
    /// A variable a shell line binds.
    Variable,
    /// A file a shell line writes through a redirect.
    Write,
    /// One path outside the working directory, handed to one program.
    OutsidePath,
}

/// One key a scoped approval would record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantKey {
    /// The key itself, as the run's grants hold it.
    pub key: String,
    /// What kind of thing it names.
    pub kind: GrantKeyKind,
    /// How the option label names it.
    pub label: String,
    /// The program, for a program key and an outside-path key.
    pub program: Option<String>,
    /// The path, for a write key and an outside-path key.
    pub path: Option<String>,
}

impl GrantKey {
    /// Read `key` back into what it names.
    pub fn describe(key: &str) -> Self {
        let named = |kind, label: &str, program: Option<&str>, path: Option<&str>| Self {
            key: key.to_string(),
            kind,
            label: label.to_string(),
            program: program.map(str::to_string),
            path: path.map(str::to_string),
        };
        if let Some((program, path)) = outside_path_of(key) {
            let label = format!("{program} with {path} (outside the working directory)");
            return named(
                GrantKeyKind::OutsidePath,
                &label,
                Some(&program),
                Some(&path),
            );
        }
        let Some(shell) = key.strip_prefix(SHELL_PREFIX) else {
            return named(GrantKeyKind::Tool, key, None, None);
        };
        if shell.starts_with("env:") {
            return named(GrantKeyKind::Variable, shell, None, None);
        }
        match shell.strip_prefix('>') {
            Some(path) => named(GrantKeyKind::Write, shell, None, Some(path)),
            None => named(GrantKeyKind::Program, shell, Some(shell), None),
        }
    }
}

/// Why a shell line records nothing when it is approved for a stage or a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantBlockerReason {
    /// A word names a path outside the working directory where no key can
    /// name it: in an option, a redirect, a URL or a `cd`.
    OutsideWorkdir,
    /// A word is expanded by the shell at run time, so what it will be is not
    /// in the line.
    ExpandedAtRunTime,
    /// The program is named through a variable or a substitution.
    ProgramFromVariable,
    /// A word names paths that cannot be listed ahead of time: a glob or a
    /// brace list that reaches outside, another user's home, anything after
    /// the line leaves the working directory.
    UnplaceablePath,
    /// The program runs a command put together at run time (`eval`,
    /// `source`).
    RunsAssembledCommand,
    /// The word installs code to run later (`trap`, `alias`, `function`).
    InstallsCode,
    /// A flag lets an otherwise read-only program write a file or run
    /// another program.
    EscapeFlag,
    /// The line cannot be read as a list of commands.
    UnreadableLine,
    /// Nothing in the line runs a program.
    NothingRuns,
}

/// One word of a shell line that keeps the line from being granted, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantBlocker {
    /// The word, as the line spells it with its quotes removed. Empty when no
    /// single word is to blame.
    pub word: String,
    /// Why it cannot be granted.
    pub reason: GrantBlockerReason,
    /// The same, as one clause a person can read.
    pub detail: String,
}

/// What approving a tool call for its stage or for the run would record: every
/// key, for each scope, and what keeps the call from recording any.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalGrant {
    /// Every key an approval for the stage records.
    pub stage: Vec<GrantKey>,
    /// Every key an approval for the run records.
    pub run: Vec<GrantKey>,
    /// Why an approval records nothing, one entry per word that stops it.
    /// Empty when it records something, or when there is nothing to say.
    pub blocked: Vec<GrantBlocker>,
}

impl ApprovalGrant {
    /// The grant `keys` describes.
    pub fn of(keys: &GrantKeys) -> Self {
        let describe = |keys: &[String]| keys.iter().map(|k| GrantKey::describe(k)).collect();
        Self {
            stage: describe(&keys.stage),
            run: describe(&keys.run),
            blocked: keys.blocked.clone(),
        }
    }

    /// The label of the option that approves the call for `scope` ("stage" or
    /// "run"), naming every key in `keys`, which is this grant's list for that
    /// scope. With no key it says the approval is for this call alone, and why
    /// when a reason is known.
    pub(super) fn scope_label(&self, scope: &str, keys: &[GrantKey]) -> String {
        if !keys.is_empty() {
            let named: Vec<&str> = keys.iter().map(|k| k.label.as_str()).collect();
            return format!("Allow {} for this {scope}", named.join(", "));
        }
        if self.blocked.is_empty() {
            return format!("Allow for this {scope} (nothing reusable - it will ask again)");
        }
        let why: Vec<&str> = self.blocked.iter().map(|b| b.detail.as_str()).collect();
        format!(
            "Allow for this {scope} (nothing reusable: {} - it will ask again)",
            why.join("; ")
        )
    }
}

#[cfg(test)]
#[path = "interaction_grant_tests.rs"]
mod tests;
