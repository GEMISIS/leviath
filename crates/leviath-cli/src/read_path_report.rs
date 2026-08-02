//! Whether the user's config actually grants what a blueprint's
//! `[read_paths]` declares, entry by entry.
//!
//! Declaring is not granting (see [`leviath_core::read_paths`]), and for a long
//! time nothing said so out loud: an agent that asked to read outside its
//! workdir validated, listed, and spawned exactly like one that did not, then
//! failed at its first read on any machine whose `config.toml` was missing the
//! grant. This module is the one place that answers "is this declaration live
//! here", so `lev validate`, `lev list`, `lev run`, `lev add`, and `lev ps` all
//! answer it the same way.
//!
//! The check is deliberately pattern-level: each declared entry is compiled,
//! reduced to one representative path
//! ([`leviath_core::ReadPathEntry::sample_path`]), and offered to the compiled
//! grant set with [`leviath_core::ReadPathSet::matches_lexically`]. Nothing
//! touches the filesystem, so a grant naming a directory that does not exist
//! yet still reads as a grant. The trade is that a report is not a promise: the
//! runtime matches real, symlink-resolved paths, so an individual read can
//! still be refused.

use crate::config::Config;
use std::path::Path;

/// Whether one declared entry is live under the current config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    /// The config grants it (itemized, or through the blanket override).
    Granted,
    /// Nothing in the config grants it; reads matching it will be refused.
    NotGranted,
    /// The entry's pattern admits no representative path, so this cannot be
    /// answered without guessing. Reported as unknown rather than as inert.
    Undetermined,
}

/// One declared entry and its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryStatus {
    /// The entry exactly as the blueprint wrote it.
    pub raw: String,
    /// Whether the config grants it.
    pub status: GrantStatus,
}

/// Every `[read_paths]` entry a blueprint declares, with its grant verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantReport {
    /// The agent name, which is also the `[agent_read_paths.<name>]` key.
    pub agent: String,
    /// Whether `[security] allow_blueprint_read_paths` is on, in which case
    /// every declaration is granted wholesale.
    pub allow_blueprint: bool,
    /// One entry per declaration, in the order the blueprint wrote them.
    pub entries: Vec<EntryStatus>,
}

/// Build the report for `blueprint` under `config`.
///
/// `None` when the blueprint declares nothing - the overwhelmingly common case,
/// where every surface should stay silent. `Err` when the *user's own* grant
/// list does not compile: that is worth saying out loud, because the same list
/// is a hard spawn error.
///
/// `workdir` resolves relative entries, exactly as it does at spawn. Callers
/// outside a run pass the current directory, which is what `lev run` defaults
/// to.
pub fn build(
    blueprint: &leviath_core::Blueprint,
    config: &Config,
    workdir: &Path,
) -> Option<Result<GrantReport, String>> {
    let rp = blueprint
        .read_paths
        .as_ref()
        .filter(|rp| !rp.allow.is_empty())?;
    Some(report_entries(
        &blueprint.name,
        &rp.allow,
        config,
        workdir,
        leviath_core::home_dir().as_deref(),
        cfg!(windows),
    ))
}

/// The report proper, with the platform inputs injected so every branch is
/// testable on every OS.
fn report_entries(
    agent: &str,
    declared: &[String],
    config: &Config,
    workdir: &Path,
    home: Option<&Path>,
    windows: bool,
) -> Result<GrantReport, String> {
    let grant_entries = config.read_path_grants_for_agent(agent);
    let grants = leviath_core::ReadPathSet::compile(&grant_entries, workdir, home, windows)
        .map_err(|e| format!("read_paths grant in your config.toml: {e}"))?;
    let allow_blueprint = config.security.allow_blueprint_read_paths;
    let entries = declared
        .iter()
        .map(|raw| EntryStatus {
            raw: raw.clone(),
            status: entry_status(raw, &grants, allow_blueprint, workdir, home, windows),
        })
        .collect();
    Ok(GrantReport {
        agent: agent.to_string(),
        allow_blueprint,
        entries,
    })
}

/// The verdict for one declared entry.
///
/// A declaration that does not compile is [`GrantStatus::Undetermined`] rather
/// than an error: the manifest parser already refuses malformed entries, so
/// reaching this with one means the environment (a missing home directory, say)
/// is what could not be resolved, and that is not something to report as
/// "ungranted".
fn entry_status(
    raw: &str,
    grants: &leviath_core::ReadPathSet,
    allow_blueprint: bool,
    workdir: &Path,
    home: Option<&Path>,
    windows: bool,
) -> GrantStatus {
    if allow_blueprint {
        return GrantStatus::Granted;
    }
    if grants.is_empty() {
        return GrantStatus::NotGranted;
    }
    let one = [raw.to_string()];
    let sample = leviath_core::ReadPathSet::compile(&one, workdir, home, windows)
        .ok()
        .and_then(|set| set.entries().first().and_then(|e| e.sample_path()));
    match sample {
        Some(sample) if grants.matches_lexically(&sample) => GrantStatus::Granted,
        Some(_) => GrantStatus::NotGranted,
        None => GrantStatus::Undetermined,
    }
}

impl GrantReport {
    /// How many entries the blueprint declares.
    pub fn declared(&self) -> usize {
        self.entries.len()
    }

    /// How many of them the config grants.
    pub fn granted(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.status == GrantStatus::Granted)
            .count()
    }

    /// The entries that will be refused, in declaration order. Undetermined
    /// entries are left out: they may well work, and offering to grant one the
    /// user already granted would be worse than saying nothing.
    pub fn ungranted(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| e.status == GrantStatus::NotGranted)
            .map(|e| e.raw.as_str())
            .collect()
    }

    /// Whether anything is refused, which is what every surface warns about.
    pub fn has_ungranted(&self) -> bool {
        !self.ungranted().is_empty()
    }

    /// `"3 declared, 1 granted"` - the one-line count for compact surfaces.
    pub fn summary(&self) -> String {
        format!("{} declared, {} granted", self.declared(), self.granted())
    }

    /// The config stanza that would grant everything currently refused, ready
    /// to paste. Empty when nothing is refused.
    pub fn grant_stanza(&self) -> Vec<String> {
        let ungranted = self.ungranted();
        if ungranted.is_empty() {
            return Vec::new();
        }
        let listed = ungranted
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", ");
        vec![
            format!("[agent_read_paths.{}]", self.agent),
            format!("allow = [{listed}]"),
        ]
    }

    /// The full report block: the counts, then one line per entry, then the
    /// stanza to paste. Indented by `indent` so each surface can place it.
    pub fn report_lines(&self, indent: &str) -> Vec<String> {
        let mut lines = vec![format!(
            "{indent}declares [read_paths] (reads outside the run workdir): {}",
            self.summary()
        )];
        if self.allow_blueprint {
            lines.push(format!(
                "{indent}  all granted by [security] allow_blueprint_read_paths = true"
            ));
        }
        for entry in &self.entries {
            let verdict = match entry.status {
                GrantStatus::Granted => "granted",
                GrantStatus::NotGranted => "NOT granted - reads matching it will be refused",
                GrantStatus::Undetermined => "cannot be checked from the pattern alone",
            };
            lines.push(format!("{indent}  {}: {verdict}", entry.raw));
        }
        if self.has_ungranted() {
            lines.push(format!("{indent}Add to your config.toml:"));
            lines.extend(
                self.grant_stanza()
                    .into_iter()
                    .map(|l| format!("{indent}  {l}")),
            );
        }
        lines
    }

    /// The one-line warning for surfaces that only have room for one, `None`
    /// when nothing is refused.
    pub fn warning_line(&self) -> Option<String> {
        self.has_ungranted().then(|| {
            format!(
                "warning: agent '{}' declares [read_paths] your config does not grant ({}); \
                 reads outside the workdir will be refused",
                self.agent,
                self.summary()
            )
        })
    }
}

#[cfg(test)]
mod tests;
