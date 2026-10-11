//! Hot-reloading of the daemon's spawn-time config.
//!
//! [`ConfigReloader`] owns the config an agent reads *at spawn* (permissions,
//! `[read_paths]`, sandbox defaults, limits, taint), so a `config.toml` edit is
//! in force on the next `lev run` rather than the next daemon restart. It
//! reloads the file when its mtime changes, mirroring the script-provider
//! hot-reload (`leviath_runtime::script_provider`), and keeps the last good
//! config if a reload fails so an edit saved mid-keystroke never breaks a
//! spawn.
//!
//! Looking at the file and reading the config are two different calls. A
//! [`check`](ConfigReloader::check) is a `stat`, and after a save a read, a
//! parse and, with keys in the OS keychain, a keychain read that can wait on
//! a prompt; it runs on tokio's blocking pool, before each spawn and each
//! `lev serve` request ([`checked`](ConfigReloader::checked)) and every
//! couple of seconds from the daemon's own watcher. Reading what is in force
//! ([`in_force`](ConfigReloader::in_force)) never touches the disk, which is
//! what the world loop every run's tick waits on can afford.
//!
//! Provider credentials are rebuilt from the same reloaded config by the
//! `provider_reload` module beside this one, and `[observability]` by the
//! `telemetry_reload` module next to that. Settings that live somewhere other
//! than the spawn-time config follow it through
//! [`ConfigReloader::with_reload_hook`]: a caller registers what to re-apply,
//! and it runs on each successful reload. The daemon uses it for the
//! process-wide network policy, which is mirrored into atomics the shared
//! blocking HTTP client reads (`script_host::mirror_process_policy`).
//!
//! The global `[[mcp_servers]]` are not reloaded here, because bringing a
//! server up or down is async and this reload is read from the sync spawner.
//! The `mcp_reload` module beside this one reconciles them from the spawn
//! preprocessor, the one hook on the spawn path that can await, so a server
//! added, edited or removed reaches the next run too. What still comes from the
//! config the daemon booted with is `[limits] mcp_idle_disconnect_secs`, which
//! the MCP pool is built with; see `daemon.md`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use crate::config::{Config, ConfigFault};

/// The config as of a given file mtime.
struct Cached {
    /// The mtime this reloader last *looked at*, good or bad. `None` means the
    /// file did not exist then (defaults in use); it reloads if the file later
    /// appears.
    ///
    /// Advanced on a failed load as well as a successful one, which is what
    /// makes a broken file cost one stat per spawn instead of a stat, a read,
    /// a parse and a log line.
    mtime: Option<SystemTime>,
    /// The mtime of the config actually in force. Differs from
    /// [`mtime`](Self::mtime) exactly while the file on disk is broken, and is
    /// what "the last good config" means as a fact a client can check.
    loaded_mtime: Option<SystemTime>,
    config: Arc<Config>,
    /// Why the file on disk does not load, while it does not.
    fault: Option<ConfigFault>,
    /// When [`fault`](Self::fault) was first seen, so a surface can say how
    /// long the config has been broken rather than only that it is.
    since: Option<SystemTime>,
}

/// Whether `config.toml` loads, and what is running while it does not.
///
/// A save that does not parse keeps the last good config in force, and that is
/// invisible from outside: the `tracing::warn!` goes to the daemon log, so a
/// user with a typo just watches their edits do nothing. This is that fact in a
/// shape the API, the dashboard and the CLI can each render.
#[derive(Debug, Clone)]
pub(crate) struct ConfigHealth {
    /// The file being watched.
    pub(crate) path: PathBuf,
    /// `None` while the file on disk loads.
    pub(crate) fault: Option<ConfigFault>,
    /// When the fault was first seen.
    pub(crate) since: Option<SystemTime>,
    /// The mtime of the config in force. While `fault` is set this is the last
    /// good save, not what is on disk.
    pub(crate) loaded_mtime: Option<SystemTime>,
    /// The config in force, handed back with the health rather than fetched
    /// beside it: a caller that asked both questions separately could catch a
    /// save landing between the two and answer with a config and a verdict
    /// that disagree about whether it had been seen.
    pub(crate) config: Arc<Config>,
}

impl ConfigHealth {
    /// Whether the file on disk loads.
    pub(crate) fn is_healthy(&self) -> bool {
        self.fault.is_none()
    }
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
                loaded_mtime: mtime,
                config: Arc::new(initial),
                fault: None,
                since: None,
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
                loaded_mtime: None,
                config: Arc::new(config),
                fault: None,
                since: None,
            }),
            on_reload: None,
        }
    }

    /// The config in force as of the last [`check`](Self::check), without
    /// looking at the file.
    ///
    /// Memory only, so it is what code that must not wait on a disk reads:
    /// the world loop every run's tick goes through, and anything else that
    /// cannot await. How fresh it is depends on who checks: the daemon and
    /// `lev serve` each check on a timer, and a caller that has to see a save
    /// made a moment ago asks [`checked`](Self::checked) instead.
    pub(crate) fn in_force(&self) -> Arc<Config> {
        self.lock().config.clone()
    }

    /// Look at the file now, reload it if it was saved since the last look,
    /// and say what is in force.
    ///
    /// A reload that fails to parse (an edit saved half-written, a syntax
    /// error) does not fail the caller: it records the fault and keeps the
    /// last good config, so a broken file degrades to "your last saved
    /// config" rather than a broken spawn, and the health says so. The broken
    /// file's mtime is recorded either way, so the warning fires once per bad
    /// save rather than on every check, and the next save moves the mtime
    /// again and is read.
    ///
    /// Blocking: one `stat` when nothing changed, and a read, a parse and,
    /// with keys in the OS keychain, a keychain read when something did. An
    /// async caller goes through [`checked`](Self::checked).
    pub(crate) fn check(&self) -> ConfigHealth {
        let cached = self.refresh();
        self.health_of(&cached)
    }

    /// [`check`](Self::check) on tokio's blocking pool, for an async caller
    /// that has to see a save made a moment ago: a spawn, or a `lev serve`
    /// request.
    pub(crate) async fn checked(self: Arc<Self>) -> ConfigHealth {
        crate::blocking::blocking(move || self.check()).await
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cached> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn health_of(&self, cached: &Cached) -> ConfigHealth {
        ConfigHealth {
            // A fixed reloader watches no file and can never be unhealthy; the
            // empty path says "there is nothing being watched here".
            path: self.path.clone().unwrap_or_default(),
            fault: cached.fault.clone(),
            since: cached.since,
            loaded_mtime: cached.loaded_mtime,
            config: cached.config.clone(),
        }
    }

    /// Re-read the file if its mtime moved, and hand back the cache to read.
    ///
    /// The whole state machine lives here so a check and a health report
    /// cannot disagree about whether a save has been seen.
    fn refresh(&self) -> std::sync::MutexGuard<'_, Cached> {
        let mut cached = self.lock();
        let Some(path) = &self.path else {
            // A fixed reloader: nothing to watch.
            return cached;
        };
        let mtime = file_mtime(path);
        if mtime == cached.mtime {
            // Includes the broken case: the failing mtime is recorded below,
            // so a file that will not parse is stat'd once per call and read,
            // parsed and logged exactly once per save.
            return cached;
        }
        // Bind the displayed path in a plain statement rather than as a lazy
        // `%path.display()` tracing field: the method-call region inside a
        // structured field is only reached when the callsite is enabled, and
        // tracing caches callsite interest process-globally, so it is
        // unreachable under a coverage run whose other tests hit it with no
        // subscriber. A pre-bound value sidesteps that.
        let displayed = path.display();
        cached.mtime = mtime;
        match Config::load_from_path_faulted(path) {
            Ok(config) => {
                let config = Arc::new(config);
                cached.loaded_mtime = mtime;
                cached.config = config.clone();
                // Under the cache lock on purpose: the hook re-applies settings
                // that have been copied elsewhere, and two threads reloading at
                // once must not install their configs in one order and their
                // side effects in the other.
                if let Some(hook) = &self.on_reload {
                    hook(&config);
                }
                // Recovery is news in its own right: the operator wants to
                // know their fix took, and a client watching health needs the
                // edge to clear its banner.
                match cached.fault.take() {
                    Some(_) => {
                        cached.since = None;
                        tracing::info!(
                            path = %displayed,
                            "config parses again; reloaded and back on the file on disk"
                        );
                    }
                    None => {
                        tracing::info!(path = %displayed, "reloaded config after an on-disk change")
                    }
                }
            }
            Err(fault) => {
                // Keep the config already in force; only the "mtime last
                // looked at", recorded above whichever way this went, moves.
                // That is what makes the warning fire once per broken save
                // rather than once per spawn and once per `lev serve` request.
                // The *next* save moves the mtime again, so a fixed file still
                // reloads.
                let summary = fault.summary();
                let kind = fault.kind.as_str();
                if cached.fault.is_none() {
                    cached.since = Some(SystemTime::now());
                }
                cached.fault = Some(*fault);
                tracing::warn!(
                    path = %displayed,
                    kind,
                    error = %summary,
                    "config changed on disk but failed to load; keeping the last good config"
                );
            }
        }
        cached
    }
}

/// Watches `config.toml` only to report whether it loads.
///
/// The read-only half of [`ConfigReloader`], for the surfaces that have to
/// *say* the file is broken without serving anything from it: the dashboard
/// header, redrawn ten times a second, and `lev doctor`. It keeps no config
/// and logs nothing, which is what makes it safe to poll from inside a draw
/// loop.
///
/// mtime-gated like the reloader, for the same reason: a poll on a file that
/// has not been saved is one `stat`, so a tick can afford to ask every time.
pub(crate) struct ConfigWatch {
    path: PathBuf,
    /// The mtime last judged. `None` before the first poll, which is why
    /// [`checked`](Self::checked) exists rather than treating `None` as "no
    /// file".
    mtime: Option<SystemTime>,
    checked: bool,
    fault: Option<ConfigFault>,
}

impl ConfigWatch {
    /// Watch `path`, without reading it yet. The first
    /// [`poll`](Self::poll) does that.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            mtime: None,
            checked: false,
            fault: None,
        }
    }

    /// Why the file does not load, re-reading it only if it has been saved
    /// since the last call.
    pub(crate) fn poll(&mut self) -> Option<&ConfigFault> {
        let mtime = file_mtime(&self.path);
        if self.checked && mtime == self.mtime {
            return self.fault.as_ref();
        }
        self.checked = true;
        self.mtime = mtime;
        self.fault = ConfigFault::check(&self.path);
        self.fault.as_ref()
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
        set_mtime(path, later);
    }

    /// Pin a file's mtime to an exact instant, so a test can rewrite the
    /// contents *without* the reloader seeing a new save.
    fn set_mtime(path: &std::path::Path, at: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(at).unwrap();
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

        let a = reloader.check().config;
        let b = reloader.check().config;
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
                .check()
                .config
                .read_path_grants_for_agent("cto")
                .is_empty()
        );

        // The user grants a read path and saves.
        write(&path, &config_with_grant("cto", "~/.leviath/runs"));
        bump_mtime(&path);

        // Subscriber active so the "reloaded config" info-log field evaluates.
        let reloaded = reloader.check().config;
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
                .check()
                .config
                .read_path_grants_for_agent("cto")
                .is_empty()
        );

        write(&path, &config_with_grant("cto", "~/docs"));
        let reloaded = reloader.check().config;
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
        let reloader = ConfigReloader::new(path.clone(), Config::load_from_path(&path).unwrap());
        assert_eq!(
            reloader.check().config.read_path_grants_for_agent("cto"),
            vec!["~/good".to_string()]
        );

        // A half-saved, unparseable edit.
        write(&path, "this is not valid : : toml");
        bump_mtime(&path);

        // Subscriber active so the "failed to reload" warn-log field evaluates.
        let after = reloader.check().config;
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
        let _ = reloader.check().config;
        assert!(recorded().is_empty());

        let mut tightened = Config::default();
        tightened.security.allow_local_network = true;
        write(&path, &toml::to_string(&tightened).unwrap());
        bump_mtime(&path);
        let _ = reloader.check().config;
        // Read again with the file unchanged: still one call.
        let _ = reloader.check().config;
        assert_eq!(
            recorded(),
            vec![true],
            "the hook runs once per change, with the config that changed"
        );

        // A half-saved edit keeps the last good config, so it keeps the last
        // good side effects too.
        write(&path, "not : : toml");
        bump_mtime(&path);
        let _ = reloader.check().config;
        assert_eq!(recorded(), vec![true], "a broken save applies nothing");
    }

    /// A file left broken is read once, not on every spawn: the watched mtime
    /// advances even though the load failed. Keeping the stale mtime instead
    /// would cost `lev serve` a read, a parse and a warning per page load.
    #[test]
    fn a_file_left_broken_is_not_re_read_on_every_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let reloader = ConfigReloader::new(path.clone(), Config::load_from_path(&path).unwrap());
        let good = reloader.check().config;

        write(&path, "broken : :");
        bump_mtime(&path);
        let first = reloader.check().config;
        assert!(
            Arc::ptr_eq(&first, &good),
            "the broken save keeps the config already in force"
        );

        // What stops the re-read: the mtime the reloader is now comparing
        // against is the broken file's, so the next `current()` short-circuits
        // on the mtime check instead of reading and parsing again.
        assert_eq!(
            reloader
                .cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .mtime,
            file_mtime(&path),
            "the broken file's mtime is recorded, which is what stops the re-read"
        );
        assert!(
            Arc::ptr_eq(&reloader.check().config, &good),
            "and the config in force is still the last good one"
        );
    }

    #[test]
    fn a_good_save_after_a_broken_one_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let reloader = ConfigReloader::new(path.clone(), Config::load_from_path(&path).unwrap());
        let _ = reloader.check().config;

        write(&path, "broken : :");
        bump_mtime(&path);
        let _ = reloader.check().config; // keeps last-good

        // The user fixes it.
        write(&path, &config_with_grant("cto", "~/fixed"));
        bump_mtime(&path);
        assert_eq!(
            reloader.check().config.read_path_grants_for_agent("cto"),
            vec!["~/fixed".to_string()]
        );
    }

    /// A reloader seeded from a file that already loads, as boot does.
    fn seeded(path: &std::path::Path) -> ConfigReloader {
        ConfigReloader::new(path.to_path_buf(), Config::load_from_path(path).unwrap())
    }

    #[test]
    fn health_goes_unhealthy_on_a_bad_save_and_healthy_again_on_a_good_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let reloader = seeded(&path);
        let good_mtime = file_mtime(&path);

        let health = reloader.check();
        assert!(health.is_healthy(), "{health:?}");
        assert_eq!(health.path, path);
        assert!(health.since.is_none());

        write(&path, "broken : :");
        bump_mtime(&path);

        let health = reloader.check();
        assert!(
            !health.is_healthy(),
            "a file that will not parse is not healthy"
        );
        assert!(health.since.is_some(), "the moment it broke is recorded");
        assert_eq!(
            health.loaded_mtime, good_mtime,
            "the config in force is still the one that loaded"
        );

        // The user fixes it.
        write(&path, &config_with_grant("cto", "~/fixed"));
        bump_mtime(&path);

        let health = reloader.check();
        assert!(health.is_healthy(), "{health:?}");
        assert!(health.since.is_none(), "recovery clears when it broke");
        assert_ne!(
            health.loaded_mtime, good_mtime,
            "a newer config is in force"
        );
    }

    #[test]
    fn a_syntax_error_reports_a_line_and_column_and_a_bad_value_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &empty_config());
        let reloader = seeded(&path);

        write(&path, "default_provider = \"anthropic\"\nbroken : :\n");
        bump_mtime(&path);
        let fault = reloader.check().fault.expect("a syntax error is a fault");
        assert_eq!(fault.line, Some(2), "{fault:?}");
        assert_eq!(fault.column, Some(8), "{fault:?}");

        write(
            &path,
            "[model_providers.local]\nkind = \"openai-compatible\"\n",
        );
        bump_mtime(&path);
        let fault = reloader
            .check()
            .fault
            .expect("an endpoint with no address is a fault");
        assert_eq!(fault.key.as_deref(), Some("model_providers.local"));
    }

    /// The reloader must read and complain about a broken file once per save,
    /// not once per spawn.
    ///
    /// Probed by rewriting the file to something valid while pinning its mtime
    /// back to the failing save's: a reloader that re-reads on every call
    /// would see the good content and go healthy, and one that remembers the
    /// mtime it already judged cannot.
    #[test]
    fn a_broken_file_is_read_once_per_failing_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let reloader = seeded(&path);

        write(&path, "broken : :");
        bump_mtime(&path);
        let broken_mtime = file_mtime(&path).expect("the file is there");
        assert!(!reloader.check().is_healthy());

        // Same mtime, different bytes: nothing has "saved", so nothing is read.
        write(&path, &config_with_grant("cto", "~/sneaked"));
        set_mtime(&path, broken_mtime);
        assert!(
            !reloader.check().is_healthy(),
            "a file already judged at this mtime must not be read again"
        );
        assert_eq!(
            reloader.check().config.read_path_grants_for_agent("cto"),
            vec!["~/good".to_string()],
            "and the config in force is still the last good one"
        );

        // A real save is a new mtime, and is picked up.
        bump_mtime(&path);
        assert!(reloader.check().is_healthy());
        assert_eq!(
            reloader.check().config.read_path_grants_for_agent("cto"),
            vec!["~/sneaked".to_string()]
        );
    }

    #[test]
    fn two_bad_saves_in_a_row_keep_the_moment_the_config_first_broke() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &empty_config());
        let reloader = seeded(&path);

        write(&path, "broken : :");
        bump_mtime(&path);
        let first = reloader.check().since.expect("it broke");

        write(&path, "still broken : :");
        bump_mtime(&path);
        assert_eq!(
            reloader.check().since,
            Some(first),
            "a second failed save does not restart the clock"
        );
    }

    #[test]
    fn the_watch_reports_a_break_and_a_fix_and_re_reads_only_on_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut watch = ConfigWatch::new(path.clone());

        // No file at all is not a fault: no config means defaults.
        assert!(watch.poll().is_none());

        write(&path, &empty_config());
        bump_mtime(&path);
        assert!(watch.poll().is_none());

        write(&path, "default_provider = \"anthropic\"\nbroken : :\n");
        bump_mtime(&path);
        let fault = watch.poll().expect("a syntax error is a fault").clone();
        assert_eq!(fault.line, Some(2));
        assert_eq!(fault.column, Some(8));

        // Same mtime, different bytes: nothing was saved, so nothing is read.
        let broken_mtime = file_mtime(&path).expect("the file is there");
        write(&path, &empty_config());
        set_mtime(&path, broken_mtime);
        assert_eq!(
            watch.poll(),
            Some(&fault),
            "a file already judged at this mtime is not read again"
        );

        // A real save clears it.
        bump_mtime(&path);
        assert!(watch.poll().is_none());
    }

    #[test]
    fn a_reloader_with_no_file_to_watch_is_always_healthy() {
        let reloader = ConfigReloader::fixed(Config::default());
        let health = reloader.check();
        assert!(health.is_healthy());
        assert_eq!(health.path, std::path::PathBuf::new());
        assert!(health.loaded_mtime.is_none());
    }

    /// Reading what is in force never looks at the file: a save, even a
    /// broken one, is seen by the next check, and only then by a read. That
    /// is what lets the world loop read the config without waiting on a disk.
    #[test]
    fn a_read_shows_the_last_check_and_a_save_waits_for_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let reloader = seeded(&path);

        write(&path, "broken : :");
        bump_mtime(&path);
        assert_eq!(
            reloader.in_force().read_path_grants_for_agent("cto"),
            vec!["~/good".to_string()]
        );

        write(&path, &config_with_grant("cto", "~/new"));
        bump_mtime(&path);
        reloader.check();
        assert_eq!(
            reloader.in_force().read_path_grants_for_agent("cto"),
            vec!["~/new".to_string()]
        );
    }

    /// The async check reads and parses on the blocking pool, so a `lev
    /// serve` request or a spawn that finds a save never parses it (or waits
    /// on a keychain prompt) on an async worker.
    #[tokio::test]
    async fn an_async_check_reloads_off_the_calling_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, &config_with_grant("cto", "~/good"));
        let seen = Arc::new(Mutex::new(None));
        let recorder = seen.clone();
        let reloader = Arc::new(seeded(&path).with_reload_hook(Box::new(move |_| {
            *recorder.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(std::thread::current().id());
        })));

        write(&path, &config_with_grant("cto", "~/new"));
        bump_mtime(&path);
        let health = reloader.clone().checked().await;
        assert_eq!(
            health.config.read_path_grants_for_agent("cto"),
            vec!["~/new".to_string()]
        );
        let reloaded_on = *seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(reloaded_on.is_some(), "the save was reloaded");
        assert_ne!(reloaded_on, Some(std::thread::current().id()));
    }
}
