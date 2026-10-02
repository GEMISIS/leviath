//! Installed blueprints in the old `agent.leviath` format become `agent.toml`
//! blueprints the first time anything looks for them.
//!
//! This build reads only `agent.toml`. A blueprint directory that holds an
//! `agent.leviath` and no `agent.toml` (everything a previous release
//! installed) is upgraded in place: a blueprint this build ships is replaced
//! by the bundled one, and any other is migrated, the way `lev blueprint
//! migrate` would, with the new file written beside its scripts. Either way
//! the old files are kept under the directory's `legacy/`, as a converted
//! run's are. A blueprint that cannot be migrated, or would lose a setting
//! the new format has no place for, is left exactly as it was and every
//! problem is named.
//!
//! The daemon does this at start for its agents directory and the
//! operator's `agent_paths`, before it converts old runs (so their workers can
//! be pinned to the new files); `lev list` and `lev run` do it too, so a
//! machine with no daemon running sees its blueprints.

use std::path::{Path, PathBuf};

/// Where an upgraded blueprint's old files go, inside its directory.
pub(crate) const LEGACY_DIR: &str = "legacy";

/// The old manifest's file name.
const OLD_MANIFEST: &str = "agent.leviath";

/// What happened to one old blueprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Migrated to an `agent.toml` beside its files.
    Migrated,
    /// A blueprint this build ships, replaced by the bundled one.
    Reinstalled,
    /// Left as it was, for these reasons.
    Failed(Vec<String>),
}

/// One old blueprint directory, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Upgraded {
    pub(crate) dir: PathBuf,
    pub(crate) outcome: Outcome,
}

impl Upgraded {
    /// The blueprint's name, as its directory is called.
    pub(crate) fn name(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Whether `dir` holds an old blueprint and no new one.
fn is_old(dir: &Path) -> bool {
    dir.join(OLD_MANIFEST).is_file() && !dir.join(leviath_blueprint::FILE_NAME).exists()
}

/// Upgrade every old blueprint in `agents_dir` (where a blueprint this build
/// ships is replaced by the bundled one) and in each of `others` (each a
/// blueprint directory, or a directory of them).
pub(crate) fn upgrade_all(agents_dir: Option<&Path>, others: &[PathBuf]) -> Vec<Upgraded> {
    let mut out = Vec::new();
    for (dir, installed) in agents_dir
        .map(|d| (d.to_path_buf(), true))
        .into_iter()
        .chain(others.iter().map(|d| (d.clone(), false)))
    {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        found.push(dir.clone());
        found.sort();
        for blueprint in found.into_iter().filter(|d| is_old(d)) {
            let bundled = installed.then(|| bundled_named(&blueprint)).flatten();
            out.push(upgrade_one(&dir, &blueprint, bundled));
        }
    }
    out
}

/// The bundled blueprint an installed directory is named after.
fn bundled_named(dir: &Path) -> Option<&'static crate::bundled::BundledAgent> {
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    crate::bundled::BUNDLED_AGENTS
        .iter()
        .find(|a| a.name == name)
}

fn upgrade_one(
    parent: &Path,
    dir: &Path,
    bundled: Option<&'static crate::bundled::BundledAgent>,
) -> Upgraded {
    let outcome = match bundled {
        Some(agent) => reinstall(parent, dir, agent),
        None => migrate(dir),
    };
    Upgraded {
        dir: dir.to_path_buf(),
        outcome: outcome.unwrap_or_else(Outcome::Failed),
    }
}

/// An error as the text a problem carries.
fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Replace an old install of a bundled blueprint with the bundled one, the
/// whole old directory kept as its `legacy/`. The bundled one is written
/// beside it first, so a failure leaves the old one where it was.
fn reinstall(
    parent: &Path,
    dir: &Path,
    agent: &'static crate::bundled::BundledAgent,
) -> Result<Outcome, Vec<String>> {
    let staging = parent.join(format!(".{}.upgrading", agent.name));
    let fresh = staging.join(agent.name);
    let done = crate::bundled::install_bundled(agent, &staging)
        .map_err(text)
        .and_then(|()| std::fs::rename(dir, fresh.join(LEGACY_DIR)).map_err(text))
        .and_then(|()| std::fs::rename(&fresh, dir).map_err(text));
    let _ = std::fs::remove_dir_all(&staging);
    done.map(|()| Outcome::Reinstalled).map_err(|e| {
        vec![format!(
            "could not replace it with the bundled blueprint: {e}"
        )]
    })
}

/// Migrate an old blueprint to an `agent.toml` beside it, its manifest moved
/// under `legacy/`.
#[cfg(feature = "legacy-runs")]
fn migrate(dir: &Path) -> Result<Outcome, Vec<String>> {
    let old = dir.join(OLD_MANIFEST);
    let manifest =
        std::fs::read_to_string(&old).map_err(|e| vec![format!("{}: {e}", old.display())])?;
    let toml = leviath_legacy_runs::migrate(&manifest)?;
    let legacy = dir.join(LEGACY_DIR);
    std::fs::create_dir_all(&legacy)
        .and_then(|()| {
            leviath_sys::write_atomic(
                &dir.join(leviath_blueprint::FILE_NAME),
                toml.as_bytes(),
                None,
            )
        })
        .and_then(|()| std::fs::rename(&old, legacy.join(OLD_MANIFEST)))
        .map_err(|e| vec![format!("could not write the migrated blueprint: {e}")])?;
    Ok(Outcome::Migrated)
}

/// Without the old-format reader an old blueprint cannot be migrated.
#[cfg(not(feature = "legacy-runs"))]
fn migrate(_dir: &Path) -> Result<Outcome, Vec<String>> {
    Err(vec![
        "this build of lev cannot read agent.leviath files (it was built without the \
         legacy-runs feature)"
            .to_string(),
    ])
}

/// Upgrade at daemon start, each outcome in the daemon's log.
pub(crate) fn upgrade_logged(agents_dir: Option<&Path>, others: &[PathBuf]) {
    for done in upgrade_all(agents_dir, others) {
        let (name, dir) = (done.name(), done.dir.display().to_string());
        match &done.outcome {
            Outcome::Migrated => {
                tracing::info!(blueprint = %name, dir = %dir, "migrated an agent.leviath blueprint to agent.toml; the old file is under legacy/")
            }
            Outcome::Reinstalled => {
                tracing::info!(blueprint = %name, dir = %dir, "replaced an old install of a bundled blueprint with this build's; the old files are under legacy/")
            }
            Outcome::Failed(problems) => {
                for problem in problems {
                    tracing::warn!(blueprint = %name, dir = %dir, problem = %problem, "an agent.leviath blueprint could not be migrated and was left as it was");
                }
            }
        }
    }
}

/// Upgrade for a command run without the daemon, each outcome on stderr.
pub(crate) fn upgrade_reported(agents_dir: Option<&Path>, others: &[PathBuf]) {
    for line in report_lines(&upgrade_all(agents_dir, others)) {
        eprintln!("{line}");
    }
}

/// What a command says about each upgrade.
fn report_lines(done: &[Upgraded]) -> Vec<String> {
    let mut out = Vec::new();
    for d in done {
        let (name, dir) = (d.name(), d.dir.display());
        match &d.outcome {
            Outcome::Migrated => out.push(format!(
                "migrated blueprint '{name}' to {dir}/agent.toml (the old agent.leviath is under legacy/)"
            )),
            Outcome::Reinstalled => out.push(format!(
                "replaced the old install of bundled blueprint '{name}' with this version's (the old files are under {dir}/legacy/)"
            )),
            Outcome::Failed(problems) => {
                out.push(format!(
                    "blueprint '{name}' at {dir} is still an agent.leviath and was left as it was; `lev blueprint migrate` it by hand:"
                ));
                out.extend(problems.iter().map(|p| format!("  - {p}")));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "blueprint_upgrade_tests.rs"]
mod tests;
