//! Hot-reloading of the daemon's spawn-time config.
//!
//! The daemon loads `~/.leviath/config.toml` once at startup and would
//! otherwise serve that snapshot for its whole life - so a user who granted a
//! `[read_paths]` path, flipped a tool permission, or changed a limit had to
//! restart the daemon before the next `lev run` saw it. That is a surprising
//! loop to be stuck in: the spawn warning tells you to edit the config, and
//! editing it appears to do nothing.
//!
//! [`ConfigReloader`] closes that gap for the config an agent reads *at spawn*
//! (permissions, `[read_paths]`, sandbox defaults, limits, taint). It reloads
//! the file when its mtime changes, mirroring the script-provider hot-reload
//! (`leviath_runtime::script_provider`), and keeps the last good config if a
//! reload fails so an edit saved mid-keystroke never breaks a spawn.
//!
//! What it deliberately does **not** reload is the infrastructure established
//! once at boot: the provider registry, MCP connections, the outbound-network
//! policy, and the telemetry sink. Those hold live connections and
//! process-wide state; re-initializing them on a file write is a much larger
//! change with its own failure modes. Adding a provider key or an MCP server
//! still needs a daemon restart; see `daemon.md`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use crate::config::Config;

/// The config as of a given file mtime.
struct Cached {
    /// The file mtime this config was loaded from. `None` means the file did
    /// not exist at load time (defaults in use); it reloads if the file later
    /// appears.
    mtime: Option<SystemTime>,
    config: Arc<Config>,
}

/// Serves the freshest spawn-time [`Config`], reloading `config.toml` when it
/// changes on disk.
pub struct ConfigReloader {
    /// The file watched for changes - [`Config::config_path`] at construction.
    /// `None` for a [`fixed`](Self::fixed) reloader that never watches a file.
    path: Option<PathBuf>,
    cache: Mutex<Cached>,
}

impl ConfigReloader {
    /// Wrap the boot-loaded `initial` config, watching `path` (normally
    /// [`Config::config_path`]). The file's current mtime is recorded so the
    /// first [`current`](Self::current) call does not reload a config that has
    /// not changed.
    pub(crate) fn new(path: PathBuf, initial: Config) -> Self {
        let mtime = file_mtime(&path);
        Self {
            path: Some(path),
            cache: Mutex::new(Cached {
                mtime,
                config: Arc::new(initial),
            }),
        }
    }

    /// A reloader that never watches a file: [`current`](Self::current) always
    /// returns `config`. For contexts that hold a config snapshot but do not
    /// hot-reload (tests, and any caller that wants a fixed config).
    #[cfg(test)]
    pub(crate) fn fixed(config: Config) -> Self {
        Self {
            path: None,
            cache: Mutex::new(Cached {
                mtime: None,
                config: Arc::new(config),
            }),
        }
    }

    /// The current spawn-time config: the cached copy when `config.toml` is
    /// unchanged, or a freshly loaded one when its mtime moved.
    ///
    /// A reload that fails to parse (an edit saved half-written, a syntax
    /// error) does not fail the caller - it logs a warning and returns the
    /// last good config, so a broken file degrades to "your last saved config"
    /// rather than a broken spawn. The stale mtime is retained, so the next
    /// successful save is picked up.
    pub(crate) fn current(&self) -> Arc<Config> {
        let Some(path) = &self.path else {
            // A fixed reloader: nothing to watch.
            return self
                .cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .config
                .clone();
        };
        let mtime = file_mtime(path);
        let mut cached = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if mtime == cached.mtime {
            return cached.config.clone();
        }
        // Bind the displayed path in a plain statement rather than as a lazy
        // `%path.display()` tracing field: the method-call region inside a
        // structured field is only reached when the callsite is enabled, and
        // tracing caches callsite interest process-globally, so it is
        // unreachable under a coverage run whose other tests hit it with no
        // subscriber. A pre-bound value sidesteps that.
        let displayed = path.display();
        match Config::load_from_path_public(path) {
            Ok(config) => {
                let config = Arc::new(config);
                cached.mtime = mtime;
                cached.config = config.clone();
                tracing::info!(path = %displayed, "reloaded config after an on-disk change");
                config
            }
            Err(e) => {
                // Keep the last-good config AND its mtime: retrying the same
                // broken file every spawn would spam the log, and we want the
                // *next* good save (a new mtime) to reload.
                tracing::warn!(
                    path = %displayed,
                    error = %e,
                    "config changed on disk but failed to reload; keeping the last good config"
                );
                cached.config.clone()
            }
        }
    }
}

/// The file's modification time, or `None` if it does not exist or cannot be
/// stat'd (treated as "no file" - a missing config means defaults).
fn file_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    /// Force a file's mtime strictly newer, so a reload is observable even when
    /// two writes land in the same clock tick (mirrors the script-provider
    /// hot-reload test helper).
    fn bump_mtime(path: &std::path::Path) {
        let later = SystemTime::now() + Duration::from_secs(5);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(later).unwrap();
    }

    /// A complete, valid config TOML (several top-level fields have no serde
    /// default, so a partial document would not parse) granting `agent` one
    /// read path.
    fn config_with_grant(agent: &str, path: &str) -> String {
        let mut c = Config::default();
        c.agent_read_paths.insert(
            agent.to_string(),
            crate::config::ReadPathGrants {
                allow: vec![path.to_string()],
            },
        );
        toml::to_string(&c).unwrap()
    }

    fn empty_config() -> String {
        toml::to_string(&Config::default()).unwrap()
    }

    #[test]
    fn an_unchanged_file_returns_the_cached_config_without_reloading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &empty_config());
        let reloader = ConfigReloader::new(path.clone(), Config::default());

        let a = reloader.current();
        let b = reloader.current();
        // Same Arc: no reload happened.
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn an_edited_file_is_reloaded_on_the_next_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &empty_config());
        let reloader = ConfigReloader::new(path.clone(), Config::default());
        assert!(
            reloader
                .current()
                .read_path_grants_for_agent("cto")
                .is_empty()
        );

        // The user grants a read path and saves.
        write(&path, &config_with_grant("cto", "~/.leviath/runs"));
        bump_mtime(&path);

        // Subscriber active so the "reloaded config" info-log field evaluates.
        let reloaded = reloader.current();
        assert_eq!(
            reloaded.read_path_grants_for_agent("cto"),
            vec!["~/.leviath/runs".to_string()],
            "the new grant must be visible without a restart"
        );
    }

    #[test]
    fn a_config_that_appears_after_boot_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // No file at construction: defaults, mtime None.
        let reloader = ConfigReloader::new(path.clone(), Config::default());
        assert!(
            reloader
                .current()
                .read_path_grants_for_agent("cto")
                .is_empty()
        );

        write(&path, &config_with_grant("cto", "~/docs"));
        let reloaded = reloader.current();
        assert_eq!(
            reloaded.read_path_grants_for_agent("cto"),
            vec!["~/docs".to_string()]
        );
    }

    #[test]
    fn a_broken_edit_keeps_the_last_good_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        // Mirror boot: the reloader is seeded with the config already on disk.
        let reloader =
            ConfigReloader::new(path.clone(), Config::load_from_path_public(&path).unwrap());
        assert_eq!(
            reloader.current().read_path_grants_for_agent("cto"),
            vec!["~/good".to_string()]
        );

        // A half-saved, unparseable edit.
        write(&path, "this is not valid : : toml");
        bump_mtime(&path);

        // Subscriber active so the "failed to reload" warn-log field evaluates.
        let after = reloader.current();
        assert_eq!(
            after.read_path_grants_for_agent("cto"),
            vec!["~/good".to_string()],
            "a broken file must not break the spawn - keep the last good config"
        );
    }

    #[test]
    fn a_good_save_after_a_broken_one_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let reloader =
            ConfigReloader::new(path.clone(), Config::load_from_path_public(&path).unwrap());
        let _ = reloader.current();

        write(&path, "broken : :");
        bump_mtime(&path);
        let _ = reloader.current(); // keeps last-good

        // The user fixes it.
        write(&path, &config_with_grant("cto", "~/fixed"));
        bump_mtime(&path);
        assert_eq!(
            reloader.current().read_path_grants_for_agent("cto"),
            vec!["~/fixed".to_string()]
        );
    }
}
