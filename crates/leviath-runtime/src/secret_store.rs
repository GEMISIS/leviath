//! The machine's secret store: where a run's secrets are kept, so that its
//! run file never holds them.
//!
//! A run's spec names each secret it uses (today, the one its webhook is
//! signed with) by a [`SecretRef`]. The secret itself is a file of its own in
//! the store, named by that reference and readable by its owner alone. The
//! store is a directory beside the runs directory, in the data root:
//! `<data root>/secrets/`. A run file copied anywhere (into a backup, a bug
//! report, another machine, an agent's context) carries only the reference.
//!
//! A run's secrets are kept for as long as the run is. A finished run can be
//! taken up again and finish again, and its webhook is posted each time it
//! does, so nothing short of deleting the run says a secret is no longer
//! needed. Deleting a run forgets its secrets ([`SecretStore::forget_run`]),
//! and a secret whose run directory is gone is swept when the daemon starts
//! ([`SecretStore::sweep`]).

use std::io;
use std::path::{Path, PathBuf};

use crate::spec::launch::Secret;
use crate::spec::names::{RunId, SecretRef};

/// A new reference for a secret of the run `run`, ending in 128 random bits.
pub fn mint(run: &RunId) -> SecretRef {
    use rand::RngExt as _;
    SecretRef::for_run(run, rand::rng().random())
}

/// The store's directory, in the data root.
pub const SECRETS_DIR: &str = "secrets";

/// A secret store: one owner-only file per secret, named by its reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretStore {
    dir: PathBuf,
}

impl SecretStore {
    /// The store of the home whose runs directory is `runs_dir`: its data
    /// root is the directory the runs are in.
    pub fn of_runs(runs_dir: &Path) -> Self {
        Self {
            dir: runs_dir.parent().unwrap_or(runs_dir).join(SECRETS_DIR),
        }
    }

    /// The store of the home the run directory `run_dir` is in.
    pub fn of_run_dir(run_dir: &Path) -> Self {
        Self::of_runs(run_dir.parent().unwrap_or(run_dir))
    }

    /// The store's directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, reference: &SecretRef) -> PathBuf {
        self.dir.join(reference.as_str())
    }

    /// Keep `secret` under `reference`: written whole or not at all, and
    /// readable by the owner alone from the moment it exists.
    pub fn keep(&self, reference: &SecretRef, secret: &Secret) -> io::Result<()> {
        leviath_sys::perms::create_private_dir_all(&self.dir).and_then(|()| {
            leviath_sys::perms::write_private(&self.path(reference), secret.expose().as_bytes())
        })
    }

    /// Keep every secret in `secrets`. When one cannot be kept, the ones
    /// kept before it are forgotten again and the error is returned.
    pub fn keep_all(&self, secrets: &[(SecretRef, Secret)]) -> io::Result<()> {
        for (i, (reference, secret)) in secrets.iter().enumerate() {
            if let Err(e) = self.keep(reference, secret) {
                secrets[..i].iter().for_each(|(r, _)| self.forget(r));
                return Err(e);
            }
        }
        Ok(())
    }

    /// The secret kept under `reference`, when the store holds it.
    pub fn read(&self, reference: &SecretRef) -> Option<Secret> {
        std::fs::read_to_string(self.path(reference))
            .ok()
            .map(Secret::new)
    }

    /// Forget the secret kept under `reference`. Forgetting one the store
    /// does not hold does nothing.
    pub fn forget(&self, reference: &SecretRef) {
        let _ = std::fs::remove_file(self.path(reference));
    }

    /// Every reference the store holds a secret under.
    fn references(&self) -> Vec<SecretRef> {
        std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| SecretRef::new(e.file_name().to_string_lossy()).ok())
            .collect()
    }

    /// Forget every secret of the run `run_id`. Returns how many there were.
    pub fn forget_run(&self, run_id: &str) -> usize {
        self.forget_where(|run| run == run_id)
    }

    /// Forget every secret whose run has no directory under `runs_dir` any
    /// more: a run deleted by hand, or one whose spawn stopped before its
    /// run file was written. Returns how many there were.
    pub fn sweep(&self, runs_dir: &Path) -> usize {
        self.forget_where(|run| !runs_dir.join(run).is_dir())
    }

    fn forget_where(&self, gone: impl Fn(&str) -> bool) -> usize {
        let doomed: Vec<SecretRef> = self
            .references()
            .into_iter()
            .filter(|r| r.run().is_some_and(&gone))
            .collect();
        doomed.iter().for_each(|r| self.forget(r));
        doomed.len()
    }

    /// Every secret the store holds, for a reader that must make sure none
    /// of them leaves the machine (`lev rage` scrubs them out of a bundle).
    pub fn secrets(&self) -> Vec<Secret> {
        self.references()
            .iter()
            .filter_map(|r| self.read(r))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SecretStore, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let runs = home.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let store = SecretStore::of_runs(&runs);
        (home, store, runs)
    }

    fn minted(run: &str) -> SecretRef {
        mint(&RunId::new(run).unwrap())
    }

    #[test]
    fn the_store_sits_beside_the_runs_in_the_data_root() {
        let (home, store, runs) = store();
        assert_eq!(store.dir(), home.path().join(SECRETS_DIR));
        assert_eq!(SecretStore::of_run_dir(&runs.join("r-1")), store);
        // A runs directory with no parent keeps its store inside it.
        let root = Path::new("/");
        assert_eq!(SecretStore::of_runs(root).dir(), root.join(SECRETS_DIR));
    }

    #[test]
    fn a_kept_secret_reads_back_owner_only_and_forgets() {
        let (_home, store, _runs) = store();
        let reference = minted("r-1");
        assert_eq!(store.read(&reference), None);
        store.keep(&reference, &Secret::new("hunter2")).unwrap();
        assert_eq!(store.read(&reference), Some(Secret::new("hunter2")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(store.dir()), 0o700);
            assert_eq!(mode(&store.dir().join(reference.as_str())), 0o600);
        }
        store.forget(&reference);
        store.forget(&reference);
        assert_eq!(store.read(&reference), None);
    }

    #[test]
    fn keeping_several_is_all_or_nothing() {
        let (home, store, _runs) = store();
        let first = minted("r-1");
        store
            .keep_all(&[(first.clone(), Secret::new("a"))])
            .unwrap();
        assert_eq!(store.secrets(), vec![Secret::new("a")]);
        store.forget(&first);
        // A directory where the second file would go: the first is taken
        // back out.
        let second = minted("r-1");
        std::fs::create_dir_all(store.dir().join(second.as_str()).join("x")).unwrap();
        let both = [
            (first.clone(), Secret::new("a")),
            (second, Secret::new("b")),
        ];
        assert!(store.keep_all(&both).is_err());
        assert_eq!(store.read(&first), None);
        // A store that cannot be made at all.
        std::fs::write(home.path().join("file-not-dir"), b"x").unwrap();
        let unmade = SecretStore::of_runs(&home.path().join("file-not-dir").join("runs"));
        assert!(unmade.keep(&first, &Secret::new("a")).is_err());
    }

    #[test]
    fn deleting_a_run_forgets_its_secrets_and_a_sweep_takes_the_orphans() {
        let (_home, store, runs) = store();
        let mine = [minted("r-1"), minted("r-1")];
        let other = minted("r-10");
        let orphan = minted("gone");
        std::fs::create_dir_all(runs.join("r-1")).unwrap();
        std::fs::create_dir_all(runs.join("r-10")).unwrap();
        for r in mine.iter().chain([&other, &orphan]) {
            store.keep(r, &Secret::new("s")).unwrap();
        }
        // Not a minted name: neither forgotten nor swept.
        let stray = SecretRef::new("not-minted").unwrap();
        store.keep(&stray, &Secret::new("s")).unwrap();
        assert_eq!(store.sweep(&runs), 1);
        assert_eq!(store.read(&orphan), None);
        assert_eq!(store.forget_run("r-1"), 2);
        assert_eq!(store.forget_run("r-1"), 0);
        assert!(store.read(&other).is_some() && store.read(&stray).is_some());
        // A store that was never made forgets nothing.
        let empty = SecretStore::of_runs(&runs.join("nowhere").join("runs"));
        assert_eq!(empty.forget_run("r-1") + empty.sweep(&runs), 0);
    }
}
