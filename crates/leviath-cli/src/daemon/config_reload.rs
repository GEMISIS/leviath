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
//! Provider credentials are rebuilt from the same reloaded config by the
//! `provider_reload` module beside this one, so a key added or removed here
//! reaches the next run too, and `[observability]` is rebuilt by the
//! `telemetry_reload` module next to that. Settings that live somewhere other
//! than the spawn-time config follow it through
//! [`ConfigReloader::with_reload_hook`]: a caller registers what to re-apply,
//! and it runs on each successful reload. The daemon uses it for the
//! process-wide network policy, which is mirrored into atomics the shared
//! blocking HTTP client reads (`script_host::mirror_process_policy`).
//!
//! What is still established once at boot is MCP connections: those hold live
//! connections, and adding an MCP server still needs a daemon restart; see
//! `daemon.md`.

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

/// What to re-apply when `config.toml` reloads, for settings that live
/// outside the config an agent reads at spawn.
pub(crate) type ReloadHook = Box<dyn Fn(&Config) + Send + Sync>;

/// Serves the freshest spawn-time [`Config`], reloading `config.toml` when it
/// changes on disk.
pub struct ConfigReloader {
    /// The file watched for changes - [`Config::config_path`] at construction.
    /// `None` for a [`fixed`](Self::fixed) reloader that never watches a file.
    path: Option<PathBuf>,
    cache: Mutex<Cached>,
    /// Run on each *successful* reload, with the config just loaded. `None`
    /// for a caller with nothing outside the config to keep in step - which is
    /// every caller but the daemon, and is why this is opt-in rather than
    /// wired in here: the hook the daemon installs writes process-wide
    /// atomics, and a test that stood up a host would otherwise write them
    /// too, under every other test in the binary.
    on_reload: Option<ReloadHook>,
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
            on_reload: None,
        }
    }

    /// Run `hook` with the new config every time this reloader picks up a
    /// change, so a setting that has been copied somewhere else - a
    /// process-wide atomic, a live resource - follows the file down as well as
    /// up. Not called for the config the reloader was constructed with: that
    /// one the caller already has, and applies itself.
    pub(crate) fn with_reload_hook(mut self, hook: ReloadHook) -> Self {
        self.on_reload = Some(hook);
        self
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
            on_reload: None,
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
                // Under the cache lock on purpose: the hook re-applies settings
                // that have been copied elsewhere, and two threads reloading at
                // once must not install their configs in one order and their
                // side effects in the other.
                if let Some(hook) = &self.on_reload {
                    hook(&config);
                }
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

    /// A setting the daemon has copied somewhere else - the process-wide
    /// network atomics - has to follow the file, and the reloader is where
    /// that is noticed. The hook fires once per successful reload: on every
    /// read it would be pointless work, on none of them `allow_local_network`
    /// would stay at its boot value for the daemon's life, and on a broken
    /// save it would re-apply a policy parsed out of half a file.
    #[test]
    fn the_reload_hook_runs_once_per_good_save_and_never_on_a_bad_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &empty_config());
        let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let reloader = ConfigReloader::new(path.clone(), Config::default()).with_reload_hook(
            Box::new(move |config| {
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(config.security.allow_local_network);
            }),
        );
        let recorded = || seen.lock().unwrap_or_else(PoisonError::into_inner).clone();

        // The config it was built with is the caller's own; no hook for it.
        let _ = reloader.current();
        assert!(recorded().is_empty());

        let mut tightened = Config::default();
        tightened.security.allow_local_network = true;
        write(&path, &toml::to_string(&tightened).unwrap());
        bump_mtime(&path);
        let _ = reloader.current();
        // Read again with the file unchanged: still one call.
        let _ = reloader.current();
        assert_eq!(
            recorded(),
            vec![true],
            "the hook runs once per change, with the config that changed"
        );

        // A half-saved edit keeps the last good config, so it keeps the last
        // good side effects too.
        write(&path, "not : : toml");
        bump_mtime(&path);
        let _ = reloader.current();
        assert_eq!(recorded(), vec![true], "a broken save applies nothing");
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
