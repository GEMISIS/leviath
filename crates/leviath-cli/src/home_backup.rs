//! A backup of what an earlier release wrote, taken before the daemon
//! changes any of it.
//!
//! When the daemon upgrades a home from an earlier release it rewrites what
//! that release left: installed blueprints become `agent.toml` files, and old
//! run directories become run files. Before it touches one, it saves it under
//! `<data root>/backups/<version>-<unix seconds>/`:
//!
//! - `agents/<name>/`: an installed blueprint's directory, copied whole.
//! - `agent_paths/<name>-<digest>/`: a blueprint from one of the operator's
//!   `agent_paths`, copied whole; the digest is of the directory it was in.
//! - `runs/<run id>/`: an old run directory, each file hard-linked (copied
//!   where a link cannot be made). Converting a run only moves its files, so
//!   the links keep them exactly as they were and cost no space while the
//!   run's own `legacy/` holds the same files. The stage logs are copied:
//!   they stay in the run's directory, where a resumed run appends to them.
//!
//! One backup is kept per release: a later start of the same release adds
//! what it changes to the backup that release began, and an item already in
//! it is not saved twice. Nothing here is ever deleted by Leviath. An item
//! that cannot be saved is not changed.

use std::path::{Path, PathBuf};

/// Where the backups of a home are, under its data root.
pub const BACKUPS_DIR: &str = "backups";

/// What a backup says about itself.
const README: &str = "README.txt";

/// What the upgrade that filled a backup did, in a backup no command has
/// told the user about yet.
const UNANNOUNCED: &str = ".unannounced";

/// How one file of a run is saved: hard-linked, or a stand-in that fails
/// where a test says.
pub type Link<'a> = &'a dyn Fn(&Path, &Path) -> std::io::Result<()>;

/// The backup this release keeps of a home.
#[derive(Debug, Clone)]
pub struct Backup {
    dir: PathBuf,
}

impl Backup {
    /// The backup of the home whose data root is `root`: the one this
    /// release began there, else a new one stamped now. Nothing is written
    /// until something is saved.
    pub fn of_home(root: &Path) -> Self {
        let backups = root.join(BACKUPS_DIR);
        let prefix = format!("{}-", env!("CARGO_PKG_VERSION"));
        let mut begun: Vec<PathBuf> = std::fs::read_dir(&backups)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.path())
            .collect();
        begun.sort();
        let dir = begun.into_iter().next().unwrap_or_else(|| {
            backups.join(format!("{prefix}{}", leviath_core::duration::now_secs()))
        });
        Self { dir }
    }

    /// The backup of the home whose runs directory is `runs_dir`: its data
    /// root is the directory the runs are in.
    pub fn of_runs(runs_dir: &Path) -> Self {
        Self::of_home(runs_dir.parent().unwrap_or(runs_dir))
    }

    /// The backup's directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Save a copy of the blueprint directory `dir` before it is changed:
    /// under `agents/` when it is in `agents_dir`, else under
    /// `agent_paths/`. Returns where the copy is.
    pub fn save_blueprint(
        &self,
        dir: &Path,
        agents_dir: Option<&Path>,
    ) -> std::io::Result<PathBuf> {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let parent = dir.parent().unwrap_or(dir);
        let rel = match agents_dir == Some(parent) {
            true => Path::new("agents").join(name.as_ref()),
            false => {
                let from =
                    leviath_runtime::spec::names::Digest::of(parent.to_string_lossy().as_bytes());
                let short: String = from.as_str().chars().take(12).collect();
                Path::new("agent_paths").join(format!("{name}-{short}"))
            }
        };
        self.save(dir, &rel, &|from, to| std::fs::copy(from, to).map(|_| ()))
    }

    /// Save the old run directory `dir` before it is converted, each file
    /// hard-linked. Returns where the copy is.
    pub fn save_run(&self, dir: &Path) -> std::io::Result<PathBuf> {
        self.save_run_with(dir, &|from, to| std::fs::hard_link(from, to))
    }

    /// [`Self::save_run`], linking each file with `link`, and copying it
    /// where that fails. The stage logs under `stages/` are always copied:
    /// they stay in the run's directory once it is converted, and a resumed
    /// run appends to them, which would change a linked copy too.
    pub fn save_run_with(&self, dir: &Path, link: Link<'_>) -> std::io::Result<PathBuf> {
        let name = dir.file_name().unwrap_or_default();
        let rel = Path::new("runs").join(name);
        let logs = dir.join("stages");
        self.save(dir, &rel, &|from, to| {
            let copy = || std::fs::copy(from, to).map(|_| ());
            match from.starts_with(&logs) {
                true => copy(),
                false => link(from, to).or_else(|_| copy()),
            }
        })
    }

    /// Save `src` at `rel` in the backup, each file with `put`, unless
    /// something is saved there already. The copy is made beside its place
    /// and moved in once whole, so a copy that stopped part way is never
    /// taken for a backup.
    fn save(&self, src: &Path, rel: &Path, put: Link<'_>) -> std::io::Result<PathBuf> {
        let dest = self.dir.join(rel);
        if dest.exists() {
            return Ok(dest);
        }
        self.begin()?;
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        let partial = dest.with_file_name(format!(".{name}.partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&partial);
        let made = copy_tree(src, &partial, put).and_then(|()| std::fs::rename(&partial, &dest));
        if let Err(e) = made {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e);
        }
        Ok(dest)
    }

    /// Make the backup's directory, saying what it is, the first time
    /// anything is saved in it.
    fn begin(&self) -> std::io::Result<()> {
        if self.dir.join(README).is_file() {
            return Ok(());
        }
        let text = format!(
            "Leviath {} saved here what it changed when it upgraded this home from an earlier \
             release, before it changed it.\n\n\
             agents/       each installed blueprint directory, as it was\n\
             agent_paths/  each blueprint from a configured agent path, as it was\n\
             runs/         each old run directory, as it was before it became a run file\n\n\
             Leviath never deletes anything here. Delete it yourself once you no longer need it.\n",
            env!("CARGO_PKG_VERSION")
        );
        leviath_sys::perms::create_private_dir_all(&self.dir).and_then(|()| {
            leviath_sys::perms::write_private(&self.dir.join(README), text.as_bytes())
        })?;
        let shown = self.dir.display().to_string();
        tracing::info!(backup = %shown, "backing up what an earlier release wrote before changing it");
        Ok(())
    }

    /// Keep `lines`, what an upgrade that saved something here did, for the
    /// next command a person runs to show them once (see [`announce`]).
    /// Nothing is kept when nothing was saved, so there is no backup to
    /// speak of.
    pub fn announce_later(&self, lines: &[String]) {
        if !self.dir.join(README).is_file() {
            return;
        }
        let path = self.dir.join(UNANNOUNCED);
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        if let Err(e) = leviath_sys::perms::write_private(&path, text.as_bytes()) {
            let (shown, why) = (path.display().to_string(), e.to_string());
            tracing::warn!(path = %shown, error = %why, "what the upgrade did could not be kept to show the user");
        }
    }
}

/// Copy the directory `src` to `dst`, each file with `put`.
fn copy_tree(src: &Path, dst: &Path, put: Link<'_>) -> std::io::Result<()> {
    std::fs::read_dir(src)
        .and_then(|entries| leviath_sys::perms::create_private_dir_all(dst).map(|()| entries))
        .and_then(|mut entries| {
            entries.try_for_each(|entry| {
                entry.and_then(|entry| {
                    let (from, to) = (entry.path(), dst.join(entry.file_name()));
                    match from.is_dir() {
                        true => copy_tree(&from, &to, put),
                        false => put(&from, &to),
                    }
                })
            })
        })
}

/// What each upgrade under the data root `root` did that no command has
/// told the user about yet, a line each. Each is told about once.
pub fn announce(root: &Path) -> Vec<String> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(root.join(BACKUPS_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join(UNANNOUNCED).is_file())
        .collect();
    found.sort();
    found
        .into_iter()
        .filter_map(|d| {
            let marker = d.join(UNANNOUNCED);
            let text = std::fs::read_to_string(&marker).unwrap_or_default();
            std::fs::remove_file(&marker).ok().map(|()| text)
        })
        .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

/// Tell the user, on stderr, what each upgrade of their home did that no
/// command has told them about yet. `lev ps` calls this, so the first look
/// at a home after its upgrade says what changed and where it was saved.
pub fn tell_once() {
    for line in leviath_core::paths::data_dir()
        .iter()
        .flat_map(|root| announce(root))
    {
        eprintln!("{line}");
    }
}

#[cfg(test)]
#[path = "home_backup_tests.rs"]
mod tests;
