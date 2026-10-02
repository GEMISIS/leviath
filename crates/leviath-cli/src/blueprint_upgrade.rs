//! Installed blueprints in the old `agent.leviath` format become `agent.toml`
//! blueprints when the daemon starts.
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
//! Before a blueprint directory is changed, the whole of it is saved in the
//! home's backup (see [`crate::home_backup`]); one that cannot be saved is
//! left as it was.
//!
//! A migrated blueprint leaves out every key the old release accepted and
//! never read. Each is a warning: in the summary of the upgrade, in the
//! daemon's log, and beside the blueprint in `lev list` and `lev validate`
//! until its owner edits it (see [`crate::upgrade_warnings`]).
//!
//! Only the daemon does this, at start, for the agents directory of the home
//! it serves and the operator's `agent_paths`, before it converts old runs (so
//! their workers can be pinned to the new files). One daemon runs per home, so
//! no two processes ever rewrite one directory at once. `lev list` and
//! `lev run` only say which blueprints are waiting and how to upgrade them.

use std::path::{Path, PathBuf};

use leviath_runtime::control_socket::StartupBoard;

use crate::daemon::upgrade::Upgrade;

/// Where an upgraded blueprint's old files go, inside its directory.
pub(crate) const LEGACY_DIR: &str = "legacy";

/// The old manifest's file name.
const OLD_MANIFEST: &str = "agent.leviath";

/// What happened to one old blueprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Migrated to an `agent.toml` beside its files.
    Migrated {
        /// A line for each setting the new file spells differently.
        notes: Vec<String>,
        /// A warning for each key the new file leaves out because nothing
        /// ever read it.
        dropped: Vec<String>,
    },
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
        name_of(&self.dir)
    }
}

/// A blueprint's name, as its directory is called.
fn name_of(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Whether `dir` holds an old blueprint and no new one.
fn is_old(dir: &Path) -> bool {
    dir.join(OLD_MANIFEST).is_file() && !dir.join(leviath_blueprint::FILE_NAME).exists()
}

/// Every old blueprint in `agents_dir` and in each of `others` (each a
/// blueprint directory, or a directory of them): the directory it sits in,
/// its own directory, and whether it is an install of `agents_dir`'s.
fn old_blueprints(agents_dir: Option<&Path>, others: &[PathBuf]) -> Vec<(PathBuf, PathBuf, bool)> {
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
            out.push((dir.clone(), blueprint, installed));
        }
    }
    out
}

/// Upgrade every old blueprint in `agents_dir` (where a blueprint this build
/// ships is replaced by the bundled one) and in each of `others`, each saved
/// in `backup` first.
pub(crate) fn upgrade_all(
    agents_dir: Option<&Path>,
    others: &[PathBuf],
    backup: &crate::home_backup::Backup,
    board: &StartupBoard,
) -> Vec<Upgraded> {
    let found = old_blueprints(agents_dir, others);
    if !found.is_empty() {
        board.begin("upgrading blueprints", found.len() as u64);
        board.detail(saving_to(backup));
    }
    found
        .into_iter()
        .enumerate()
        .map(|(i, (dir, blueprint, installed))| {
            let done = upgrade_saved(&dir, blueprint, installed, (backup, agents_dir));
            board.done(i as u64 + 1);
            done
        })
        .collect()
}

/// Upgrade the old blueprint `blueprint` in `dir` once it is saved in
/// `backup`; one that cannot be saved is left as it was.
fn upgrade_saved(
    dir: &Path,
    blueprint: PathBuf,
    installed: bool,
    (backup, agents_dir): (&crate::home_backup::Backup, Option<&Path>),
) -> Upgraded {
    if let Err(e) = backup.save_blueprint(&blueprint, agents_dir) {
        return Upgraded {
            dir: blueprint,
            outcome: Outcome::Failed(vec![format!(
                "it could not be backed up first, so it was not changed: {e}"
            )]),
        };
    }
    let bundled = installed.then(|| bundled_named(&blueprint)).flatten();
    upgrade_one(dir, &blueprint, bundled)
}

/// What the start-up board says about where the upgrade's backup goes.
pub(crate) fn saving_to(backup: &crate::home_backup::Backup) -> String {
    format!(
        "saving everything it changes to {} first",
        backup.dir().display()
    )
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

/// How a reinstall moves a directory: `std::fs::rename`, or a stand-in that
/// fails where a test says.
type Rename<'a> = &'a dyn Fn(&Path, &Path) -> std::io::Result<()>;

/// Replace an old install of a bundled blueprint with the bundled one, the
/// whole old directory kept as its `legacy/`.
fn reinstall(
    parent: &Path,
    dir: &Path,
    agent: &'static crate::bundled::BundledAgent,
) -> Result<Outcome, Vec<String>> {
    reinstall_with(parent, dir, agent, &|from, to| std::fs::rename(from, to))
}

/// [`reinstall`], moving directories with `rename`.
///
/// The bundled blueprint is written to a staging directory first, and the old
/// one is moved aside to a directory of its own, never into the staging one,
/// so nothing that is deleted ever holds the user's files. Both names carry
/// this process's id, so two upgrades of one directory never share either.
/// A step that fails puts the old directory back where it was.
fn reinstall_with(
    parent: &Path,
    dir: &Path,
    agent: &'static crate::bundled::BundledAgent,
    rename: Rename<'_>,
) -> Result<Outcome, Vec<String>> {
    let tag = std::process::id();
    let staging = parent.join(format!(".{}.upgrading-{tag}", agent.name));
    let aside = parent.join(format!(".{}.old-{tag}", agent.name));
    let fresh = staging.join(agent.name);
    let failed = |e: String| {
        vec![format!(
            "could not replace it with the bundled blueprint: {e}"
        )]
    };
    if let Err(e) = crate::bundled::install_bundled(agent, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(failed(e.to_string()));
    }
    if let Err(e) = rename(dir, &aside) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(failed(e.to_string()));
    }
    if let Err(e) = rename(&fresh, dir) {
        let back = rename(&aside, dir);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(failed(match back {
            Ok(()) => e.to_string(),
            Err(b) => format!("{e}; the old files are at {}: {b}", aside.display()),
        }));
    }
    let _ = std::fs::remove_dir(&staging);
    rename(&aside, &dir.join(LEGACY_DIR))
        .map(|()| Outcome::Reinstalled)
        .map_err(|e| {
            failed(format!(
                "the bundled one is in place, but the old files are at {}: {e}",
                aside.display()
            ))
        })
}

/// Upgrade at the start of a daemon whose runs directory is `runs_dir`. Only
/// a daemon serving its home's own runs (`home_runs`) touches the home's
/// blueprints: a host built over any other runs directory (every test's)
/// leaves them as they are.
pub(crate) fn upgrade_at_start(
    runs_dir: &Path,
    home_runs: &Path,
    (agents_dir, others): (Option<&Path>, &[PathBuf]),
    board: &StartupBoard,
    upgrade: &mut Upgrade,
) {
    if runs_dir == home_runs {
        upgrade_logged(
            agents_dir,
            others,
            &crate::home_backup::Backup::of_runs(home_runs),
            board,
            upgrade,
        );
    }
}

/// Migrate an old blueprint to an `agent.toml` beside it, its manifest moved
/// under `legacy/`.
#[cfg(feature = "legacy-runs")]
fn migrate(dir: &Path) -> Result<Outcome, Vec<String>> {
    let old = dir.join(OLD_MANIFEST);
    let manifest =
        std::fs::read_to_string(&old).map_err(|e| vec![format!("{}: {e}", old.display())])?;
    let leviath_legacy_runs::Migrated {
        text: toml,
        notes,
        dropped,
        ..
    } = leviath_legacy_runs::migrate_noted(&manifest)?;
    let dropped: Vec<String> = dropped.iter().map(ToString::to_string).collect();
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
    keep_warnings(dir, &dropped);
    Ok(Outcome::Migrated { notes, dropped })
}

/// Keep the warnings of the blueprint just upgraded in `dir` beside it, for
/// `lev list` and `lev validate` to show. A note that cannot be written is
/// said in the log: the warnings are still in the upgrade's summary.
#[cfg(feature = "legacy-runs")]
fn keep_warnings(dir: &Path, dropped: &[String]) {
    if dropped.is_empty() {
        return;
    }
    if let Err(e) = crate::upgrade_warnings::record(dir, dropped) {
        let (shown, why) = (dir.display().to_string(), e.to_string());
        tracing::warn!(dir = %shown, error = %why, "the keys an upgrade dropped could not be kept beside the blueprint");
    }
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

/// Upgrade at daemon start, each outcome in the daemon's log and added to
/// `upgrade`.
pub(crate) fn upgrade_logged(
    agents_dir: Option<&Path>,
    others: &[PathBuf],
    backup: &crate::home_backup::Backup,
    board: &StartupBoard,
    upgrade: &mut Upgrade,
) {
    // Formatted outside the macros, so the text is made whether or not a
    // subscriber reads the fields.
    let saved = backup.dir().display().to_string();
    for done in upgrade_all(agents_dir, others, backup, board) {
        let (name, dir) = (done.name(), done.dir.display().to_string());
        match &done.outcome {
            Outcome::Migrated { notes, dropped } => {
                upgrade.blueprints += 1;
                let notes = notes.join("; ");
                tracing::info!(blueprint = %name, dir = %dir, backup = %saved, notes = %notes, "migrated an agent.leviath blueprint to agent.toml; the old file is under legacy/, and the whole directory as it was is in the backup");
                for line in dropped {
                    tracing::warn!(blueprint = %name, dir = %dir, dropped = %line, "a key of the migrated blueprint was dropped");
                    upgrade.warnings.push(format!("blueprint '{name}': {line}"));
                }
            }
            Outcome::Reinstalled => {
                upgrade.blueprints += 1;
                tracing::info!(blueprint = %name, dir = %dir, backup = %saved, "replaced an old install of a bundled blueprint with this build's; the old files are under legacy/, and the whole directory as it was is in the backup")
            }
            Outcome::Failed(problems) => {
                upgrade.blueprints_failed += 1;
                for problem in problems {
                    tracing::warn!(blueprint = %name, dir = %dir, problem = %problem, "an agent.leviath blueprint could not be migrated and was left as it was");
                    upgrade.warnings.push(format!(
                        "blueprint '{name}' at {dir} was left as it was: {problem}"
                    ));
                }
            }
        }
    }
}

/// What a command run without the daemon says about each old blueprint it
/// finds. Only the daemon upgrades them: an upgrade rewrites the directory,
/// and the daemon is the one process per home that may.
pub(crate) fn pending_lines(agents_dir: Option<&Path>, others: &[PathBuf]) -> Vec<String> {
    old_blueprints(agents_dir, others)
        .into_iter()
        .map(|(_, dir, _)| {
            let shown = dir.display();
            format!(
                "blueprint '{}' at {shown} is an agent.leviath from an earlier release, so it is \
                 not listed: the daemon upgrades it when it starts (`lev daemon restart`), or \
                 convert it with `lev blueprint migrate {shown} -o {}`",
                name_of(&dir),
                dir.join(leviath_blueprint::FILE_NAME).display()
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "blueprint_upgrade_tests.rs"]
mod tests;
