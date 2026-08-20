//! Lazy, hot-reloading resolution of Rhai *script providers*.
//!
//! Native providers are built eagerly at daemon startup. Script providers are
//! not: a `.rhai` file in the providers directory becomes a live provider only
//! when an agent references its name, and it is **reloaded automatically** when
//! the file changes. [`ScriptProviderLayer`] is the seam the [`ProviderRegistry`]
//! consults for any name it doesn't have natively; it caches compiled providers
//! keyed by file mtime, so:
//!
//! - first reference to `<name>` → compile + `initialize` + cache;
//! - unchanged file → cached instance (no recompile);
//! - edited file (newer mtime) → rebuild;
//! - a brand-new file that didn't exist at startup → loads on first reference;
//! - a deleted file → evicted, the provider disappears;
//! - a broken script → not resolved (logged), so selection falls through.
//!
//! The `[model_providers.<name>]` table that feeds a script's `initialize` is
//! read the same way, through a [`ScriptProviderConfig`] source the layer calls
//! on every lookup rather than a copy taken at boot. It used to be a copy, so
//! editing a provider's `base_url` did nothing until `lev daemon restart` while
//! editing the script beside it took effect immediately - two halves of one
//! feature disagreeing, silently (issue #533).
//!
//! [`ProviderRegistry`]: crate::ProviderRegistry

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use leviath_providers::rhai_provider::host::HttpExecutor;
use leviath_providers::{ModelCapabilityOverride, Provider, RateLimitConfig, RhaiProvider};

/// Per-provider configuration from `[model_providers.<name>]`. All fields are
/// optional overrides - a script activates by an agent referencing its name and
/// the file existing, not by having an entry here.
#[derive(Clone, Debug, Default)]
pub struct ScriptProviderSpec {
    /// Script filename stem or path (default `<name>.rhai` in the providers dir).
    pub script: Option<String>,
    /// Rate limit applied by the Rust wrapper.
    pub rate_limit: Option<RateLimitConfig>,
    /// The `config` map passed to the script's `initialize` (base_url, api_key,
    /// and any extra keys), pre-assembled by the CLI.
    pub init_config: serde_json::Value,
}

/// Everything a script-provider load reads out of `config.toml`.
///
/// Grouped so the layer can take it from a *source* it calls per lookup - see
/// [`ScriptProviderLayer::with_config_source`] - rather than holding a copy.
#[derive(Clone, Debug, Default)]
pub struct ScriptProviderConfig {
    /// `[model_providers]`, keyed by provider name.
    pub overrides: HashMap<String, ScriptProviderSpec>,
    /// `[model_capabilities]`, applied to whatever a script reports.
    pub default_caps: HashMap<String, ModelCapabilityOverride>,
    /// The global request timeout a script's own calls are bounded by.
    pub request_timeout_secs: Option<u64>,
    /// `[security] allow_env_vars`: credential-shaped environment variables a
    /// provider script may read. Empty by default - a provider script runs
    /// during inference, not through a tool call, so nothing it does passes an
    /// approval prompt.
    pub env_allowlist: Arc<Vec<String>>,
}

/// A cached, compiled script provider, plus what it was built from.
///
/// Both halves matter. The mtime catches an edited script; the config catches
/// an edited `[model_providers]` entry, which used to leave a stale provider
/// cached under an unchanged file (issue #533). Compared by pointer, so a
/// source that returns the same `Arc` while nothing has changed costs nothing.
struct Cached {
    mtime: SystemTime,
    config: Arc<ScriptProviderConfig>,
    provider: Arc<dyn Provider>,
}

/// Lazy, hot-reloading resolver for script providers.
pub struct ScriptProviderLayer {
    dir: PathBuf,
    /// Where the config comes from, called on every lookup.
    ///
    /// A boxed closure rather than a captured map so the daemon can hand in
    /// one that reads its live config, while every other caller hands in a
    /// constant. `Box<dyn Fn>` rather than a generic parameter deliberately:
    /// one instantiation regardless of the caller, which keeps coverage
    /// honest (see `run/task.rs`'s `resolve_task_with` for the same choice).
    config: Box<dyn Fn() -> Arc<ScriptProviderConfig> + Send + Sync>,
    /// The HTTP executor every script provider shares, built once when the
    /// layer is created.
    ///
    /// This one really is fixed at boot, and stays that way: it holds a
    /// connection pool, which is the kind of live state the daemon's config
    /// reload deliberately leaves alone. Its timeout is the client-level
    /// default; the per-call bound in [`ScriptProviderConfig`] is live.
    ///
    /// Kept as the `Result` rather than unwrapped: constructing it reads the
    /// machine's root certificate store and can fail, and a layer is built
    /// during daemon start-up where there is nothing to return an error to. A
    /// failure therefore surfaces when a script provider is actually resolved,
    /// which is the first moment it matters.
    executor: std::result::Result<Arc<dyn HttpExecutor>, leviath_providers::provider::HttpError>,
    cache: Mutex<HashMap<String, Cached>>,
}

impl ScriptProviderLayer {
    /// Build a layer over `dir`, with per-provider `overrides`, global model
    /// capability overrides, and the global request timeout.
    pub fn new(
        dir: PathBuf,
        overrides: HashMap<String, ScriptProviderSpec>,
        default_caps: HashMap<String, ModelCapabilityOverride>,
        request_timeout_secs: Option<u64>,
        env_allowlist: Vec<String>,
    ) -> Self {
        let executor = Self::build_executor(request_timeout_secs);
        Self::with_executor(
            dir,
            overrides,
            default_caps,
            request_timeout_secs,
            env_allowlist,
            executor,
        )
    }

    /// [`new`](Self::new), with the shared HTTP executor supplied.
    ///
    /// The seam that makes the "no usable HTTPS client" path reachable: reqwest
    /// cannot be made to fail from the outside, so a test has to hand in the
    /// failure.
    pub fn with_executor(
        dir: PathBuf,
        overrides: HashMap<String, ScriptProviderSpec>,
        default_caps: HashMap<String, ModelCapabilityOverride>,
        request_timeout_secs: Option<u64>,
        env_allowlist: Vec<String>,
        executor: std::result::Result<
            Arc<dyn HttpExecutor>,
            leviath_providers::provider::HttpError,
        >,
    ) -> Self {
        let config = Arc::new(ScriptProviderConfig {
            overrides,
            default_caps,
            request_timeout_secs,
            env_allowlist: Arc::new(env_allowlist),
        });
        Self::with_config_source(dir, Box::new(move || config.clone()), executor)
    }

    /// A layer whose config is read from `config` on every lookup rather than
    /// captured once.
    ///
    /// This is what makes `[model_providers.<name>]` as hot as the `.rhai`
    /// file beside it. The source must return the *same* `Arc` while nothing
    /// has changed - the cache compares by pointer, so a source that rebuilds
    /// its answer every call recompiles every script on every lookup.
    pub fn with_config_source(
        dir: PathBuf,
        config: Box<dyn Fn() -> Arc<ScriptProviderConfig> + Send + Sync>,
        executor: std::result::Result<
            Arc<dyn HttpExecutor>,
            leviath_providers::provider::HttpError,
        >,
    ) -> Self {
        Self {
            dir,
            config,
            executor,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Build the shared HTTP executor a layer runs its scripts through. Its
    /// timeout is fixed for the life of the layer; see the field's note.
    pub fn build_executor(
        request_timeout_secs: Option<u64>,
    ) -> std::result::Result<Arc<dyn HttpExecutor>, leviath_providers::provider::HttpError> {
        // An HTTP/1.1-only twin rides along as the retry path for origins that
        // negotiate HTTP/2 and then fail every stream on it. Losing only the
        // twin is a degraded executor, not a dead one.
        leviath_providers::rhai_provider::host::executor_from_clients(
            leviath_providers::provider::build_http_client(request_timeout_secs),
            leviath_providers::provider::build_http1_client(request_timeout_secs),
        )
    }

    /// Resolve `<name>` to its script path: an explicit `script` override
    /// (an absolute path, or a stem/filename under the providers dir), else
    /// `<name>.rhai` in the providers dir.
    ///
    /// A *relative* override is confined to the providers directory. It used to
    /// be joined verbatim, so `script = "../../tools/evil"` reached outside it -
    /// and whatever it reached is compiled and run as a provider, which is the
    /// most privileged script surface there is. An absolute path is still
    /// honored: that is the documented way to point at a script kept elsewhere,
    /// and it can only come from the user's own config, not from a blueprint.
    ///
    /// `None` when the override escapes; the caller reports it and loads nothing.
    fn resolve_path(&self, name: &str, config: &ScriptProviderConfig) -> Option<PathBuf> {
        let stem = config
            .overrides
            .get(name)
            .and_then(|s| s.script.as_deref())
            .unwrap_or(name);
        let candidate = PathBuf::from(stem);
        if candidate.is_absolute() {
            return Some(candidate);
        }
        let filename = match stem.ends_with(".rhai") {
            true => stem.to_string(),
            false => format!("{stem}.rhai"),
        };
        // Reject any traversal component outright rather than normalizing it
        // away: a provider path has no legitimate reason to contain `..`, so
        // "what did the user mean by this" is the wrong question to ask.
        let joined = PathBuf::from(&filename);
        if joined
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return None;
        }
        Some(self.dir.join(joined))
    }

    /// Every script provider this layer could resolve right now, in no
    /// particular order: the `.rhai` files sitting in its directory, plus any
    /// name with a `[model_providers]` entry (which may point its `script` at a
    /// file kept elsewhere).
    ///
    /// A name here is a *candidate*, not a promise - it still has to compile.
    /// Callers that enumerate providers resolve each through
    /// [`get_or_load`](Self::get_or_load) and skip what does not load.
    ///
    /// Deliberately not folded into `ProviderRegistry::provider_names`, whose
    /// contract is "registered natively" and whose callers rely on every name
    /// it returns being `get`-able without compiling anything.
    pub fn candidate_names(&self) -> Vec<String> {
        let mut names: Vec<String> = (self.config)().overrides.keys().cloned().collect();
        // A directory that cannot be read is not an error here: it means no
        // convention-named scripts, which is the same answer as an empty one.
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "rhai")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && !names.iter().any(|n| n == stem)
                {
                    names.push(stem.to_string());
                }
            }
        }
        names
    }

    /// Get (or lazily load / reload) the provider named `name`, or `None` when
    /// there is no such script or it fails to load.
    ///
    /// The cache lock is taken three times - a read, then a write on whichever
    /// arm the compile lands on - and is **never held across
    /// `RhaiProvider::from_script`**, which parses and initializes an arbitrary
    /// user-authored `.rhai` file. That call is the slowest and least
    /// trustworthy thing this layer does; holding a process-wide lock across it
    /// serialized every agent's provider lookup behind one compile, and a panic
    /// inside it poisoned the cache for the whole daemon.
    ///
    /// The cost is that two callers racing on the same cold name may both
    /// compile it. Both get a working provider and the later `insert` wins -
    /// wasted work, never a wrong answer, and it self-corrects on the next
    /// lookup because entries are validated by mtime.
    pub fn get_or_load(&self, name: &str) -> Option<Arc<dyn Provider>> {
        // One read per lookup, used for both the path and the settings, so a
        // config that changes mid-load cannot resolve one file and configure
        // another.
        let config = (self.config)();
        let Some(path) = self.resolve_path(name, &config) else {
            tracing::warn!(
                provider = %name,
                "script provider path escapes the providers directory - refusing to load"
            );
            self.evict(name);
            return None;
        };
        let Some(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()).ok() else {
            // File gone (or unreadable): drop any stale entry, no provider.
            self.evict(name);
            return None;
        };
        if let Some(cached) = self.cached_fresh(name, mtime, &config) {
            return Some(cached);
        }

        let spec = config.overrides.get(name);
        let init_config = spec
            .map(|s| s.init_config.clone())
            .unwrap_or_else(|| serde_json::json!({}));
        let rate_limit = spec.and_then(|s| s.rate_limit.clone());
        let executor = match &self.executor {
            Ok(executor) => Arc::clone(executor),
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "no outbound HTTPS client, so script providers cannot run; \
                     leviath reads the system root certificate store at start-up"
                );
                return None;
            }
        };
        // No lock held here - see the note above.
        match RhaiProvider::from_script(
            &path,
            executor,
            leviath_providers::rhai_provider::ScriptProviderSettings {
                name: name.to_string(),
                init_config,
                caps: config.default_caps.clone(),
                rate_limit,
                request_timeout_secs: config.request_timeout_secs,
                env_allowlist: config.env_allowlist.clone(),
            },
        ) {
            Ok(p) => {
                let provider: Arc<dyn Provider> = Arc::new(p);
                self.cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        name.to_string(),
                        Cached {
                            mtime,
                            config: config.clone(),
                            provider: provider.clone(),
                        },
                    );
                Some(provider)
            }
            Err(e) => {
                self.evict(name);
                tracing::warn!(provider = %name, error = %e, "script provider load failed");
                None
            }
        }
    }

    /// The cached provider for `name`, but only if it was built from the script
    /// as it is on disk right now **and** from the config in force right now.
    /// Holds the lock just long enough to clone an `Arc`.
    fn cached_fresh(
        &self,
        name: &str,
        mtime: SystemTime,
        config: &Arc<ScriptProviderConfig>,
    ) -> Option<Arc<dyn Provider>> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let cached = cache.get(name)?;
        if cached.mtime == mtime && Arc::ptr_eq(&cached.config, config) {
            Some(cached.provider.clone())
        } else {
            None
        }
    }

    /// Drop any cached entry for `name`.
    fn evict(&self, name: &str) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const GOOD: &str = "fn initialize(config) { #{ base: config.base_url } }\n\
                        fn inference(state, request) { #{ content: \"ok\" } }";

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Force a file's mtime to be strictly newer, so a reload is observable even
    /// when two writes land in the same clock tick.
    fn bump_mtime(path: &std::path::Path) {
        let later = SystemTime::now() + Duration::from_secs(5);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(later).unwrap();
    }

    /// The config a layer is holding, for the tests that call `resolve_path`
    /// directly - the lookup path reads it once and threads it through, so the
    /// method takes it rather than reading it a second time.
    fn cfg(l: &ScriptProviderLayer) -> Arc<ScriptProviderConfig> {
        (l.config)()
    }

    fn layer(dir: PathBuf) -> ScriptProviderLayer {
        ScriptProviderLayer::new(dir, HashMap::new(), HashMap::new(), None, Vec::new())
    }

    /// The half of the feature that was frozen: the script file hot-reloads,
    /// but its `[model_providers.<name>]` table was captured at boot, so
    /// changing a `base_url` did nothing until a daemon restart - silently,
    /// with the run using the old value (issue #533).
    ///
    /// The script here reports its `base_url` as a model id, so what the
    /// provider was initialized with is directly observable.
    #[tokio::test]
    async fn a_config_change_reaches_the_next_load_without_touching_the_script() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "hot.rhai",
            "fn initialize(config) { #{ base: config.base_url } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: state.base } ] }",
        );

        // A source the test moves forward by hand, exactly as the daemon's
        // reloader does when `config.toml`'s mtime changes.
        let live: Arc<Mutex<Arc<ScriptProviderConfig>>> =
            Arc::new(Mutex::new(Arc::new(spec_config("hot", "http://first"))));
        let handle = live.clone();
        let l = ScriptProviderLayer::with_config_source(
            dir.path().to_path_buf(),
            Box::new(move || handle.lock().unwrap().clone()),
            ScriptProviderLayer::build_executor(None),
        );

        assert_eq!(first_model(&l).await, "http://first");
        // Same `Arc` back on every call: an unchanged config must be a cache
        // hit, not a recompile.
        assert!(Arc::ptr_eq(
            &l.get_or_load("hot").unwrap(),
            &l.get_or_load("hot").unwrap()
        ));

        *live.lock().unwrap() = Arc::new(spec_config("hot", "http://second"));
        assert_eq!(
            first_model(&l).await,
            "http://second",
            "the edited config must reach the next load, with the file untouched"
        );
    }

    /// One `ScriptProviderConfig` naming `provider`'s `base_url`.
    fn spec_config(provider: &str, base_url: &str) -> ScriptProviderConfig {
        let mut overrides = HashMap::new();
        overrides.insert(
            provider.to_string(),
            ScriptProviderSpec {
                init_config: serde_json::json!({ "base_url": base_url }),
                ..Default::default()
            },
        );
        ScriptProviderConfig {
            overrides,
            ..Default::default()
        }
    }

    /// The id of the first model `hot` reports - which this script sets to
    /// whatever `initialize` was handed.
    async fn first_model(l: &ScriptProviderLayer) -> String {
        let provider = l.get_or_load("hot").expect("the script loads");
        provider.list_models().await.unwrap()[0].id.clone()
    }

    #[test]
    fn candidate_names_are_the_files_on_disk_and_the_configured_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "groq.rhai", GOOD);
        write(dir.path(), "notes.txt", "not a provider");
        let mut overrides = HashMap::new();
        // Configured, and its script lives outside the directory - so only the
        // config half can name it.
        overrides.insert(
            "elsewhere".to_string(),
            ScriptProviderSpec {
                script: Some("/somewhere/else.rhai".to_string()),
                ..Default::default()
            },
        );
        // Configured *and* present by convention: named once, not twice.
        overrides.insert("groq".to_string(), ScriptProviderSpec::default());
        let l = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            overrides,
            HashMap::new(),
            None,
            Vec::new(),
        );

        let mut names = l.candidate_names();
        names.sort();
        assert_eq!(names, vec!["elsewhere".to_string(), "groq".to_string()]);
    }

    #[test]
    fn candidate_names_of_a_missing_directory_is_the_configured_set() {
        let l = layer(PathBuf::from("/no/such/providers/dir"));
        assert!(
            l.candidate_names().is_empty(),
            "an unreadable directory names nothing, the same as an empty one"
        );
    }

    #[test]
    fn loads_by_convention_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "groq.rhai", GOOD);
        let l = layer(dir.path().to_path_buf());
        let first = l.get_or_load("groq").expect("loads");
        assert_eq!(first.name(), "groq");
        // Cache hit returns the same Arc (unchanged mtime).
        let second = l.get_or_load("groq").expect("cached");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let l = layer(dir.path().to_path_buf());
        assert!(l.get_or_load("nope").is_none());
    }

    #[test]
    fn hot_reload_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "p.rhai", GOOD);
        let l = layer(dir.path().to_path_buf());
        let first = l.get_or_load("p").unwrap();
        // Rewrite with newer mtime → a fresh instance.
        write(dir.path(), "p.rhai", GOOD);
        bump_mtime(&path);
        let second = l.get_or_load("p").unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn new_file_after_construction_loads() {
        let dir = tempfile::tempdir().unwrap();
        let l = layer(dir.path().to_path_buf());
        assert!(l.get_or_load("late").is_none());
        write(dir.path(), "late.rhai", GOOD);
        assert!(l.get_or_load("late").is_some());
    }

    #[test]
    fn deleted_file_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "gone.rhai", GOOD);
        let l = layer(dir.path().to_path_buf());
        assert!(l.get_or_load("gone").is_some());
        std::fs::remove_file(&path).unwrap();
        assert!(l.get_or_load("gone").is_none());
    }

    #[test]
    fn broken_script_not_resolved() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bad.rhai", "fn inference( { oops");
        let l = layer(dir.path().to_path_buf());
        assert!(l.get_or_load("bad").is_none());
        // Not cached, so a fixed file loads on the next reference.
        let path = write(dir.path(), "bad.rhai", GOOD);
        bump_mtime(&path);
        assert!(l.get_or_load("bad").is_some());
    }

    #[test]
    fn resolve_path_honors_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let abs = dir.path().join("elsewhere.rhai");
        std::fs::write(&abs, GOOD).unwrap();
        let mut overrides = HashMap::new();
        // stem override
        overrides.insert(
            "a".to_string(),
            ScriptProviderSpec {
                script: Some("custom".to_string()),
                ..Default::default()
            },
        );
        // ".rhai" suffix override
        overrides.insert(
            "b".to_string(),
            ScriptProviderSpec {
                script: Some("custom.rhai".to_string()),
                ..Default::default()
            },
        );
        // absolute-path override
        overrides.insert(
            "c".to_string(),
            ScriptProviderSpec {
                script: Some(abs.to_string_lossy().into_owned()),
                ..Default::default()
            },
        );
        let l = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            overrides,
            HashMap::new(),
            None,
            Vec::new(),
        );
        assert_eq!(
            l.resolve_path("a", &cfg(&l)),
            Some(dir.path().join("custom.rhai"))
        );
        assert_eq!(
            l.resolve_path("b", &cfg(&l)),
            Some(dir.path().join("custom.rhai"))
        );
        assert_eq!(l.resolve_path("c", &cfg(&l)), Some(abs));
        assert_eq!(
            l.resolve_path("z", &cfg(&l)),
            Some(dir.path().join("z.rhai"))
        );
    }

    /// A relative `script` override may not climb out of the providers
    /// directory. Whatever it reached would be compiled and run as a provider -
    /// the one script surface with no permission layer in front of it - so the
    /// traversal is refused outright rather than normalized away.
    #[test]
    fn relative_script_override_cannot_escape_the_providers_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut overrides = HashMap::new();
        for (name, script) in [
            ("a", "../../tools/evil"),
            ("b", "../evil.rhai"),
            ("c", "sub/../../evil"),
        ] {
            overrides.insert(
                name.to_string(),
                ScriptProviderSpec {
                    script: Some(script.to_string()),
                    ..Default::default()
                },
            );
        }
        let l = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            overrides,
            HashMap::new(),
            None,
            Vec::new(),
        );
        for name in ["a", "b", "c"] {
            assert_eq!(
                l.resolve_path(name, &cfg(&l)),
                None,
                "{name} should be refused"
            );
            assert!(l.get_or_load(name).is_none(), "{name} must not load");
        }
    }

    /// A nested path *inside* the directory is still fine - only `..` is refused.
    #[test]
    fn nested_relative_override_inside_the_dir_still_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            "a".to_string(),
            ScriptProviderSpec {
                script: Some("vendor/custom".to_string()),
                ..Default::default()
            },
        );
        let l = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            overrides,
            HashMap::new(),
            None,
            Vec::new(),
        );
        assert_eq!(
            l.resolve_path("a", &cfg(&l)),
            Some(dir.path().join("vendor/custom.rhai"))
        );
    }

    #[test]
    fn init_config_reaches_the_script() {
        let dir = tempfile::tempdir().unwrap();
        // Script echoes config.base_url into state; inference returns it.
        write(
            dir.path(),
            "echo.rhai",
            "fn initialize(config) { #{ b: config.base_url } }\n\
             fn inference(state, request) { #{ content: state.b } }",
        );
        let mut overrides = HashMap::new();
        overrides.insert(
            "echo".to_string(),
            ScriptProviderSpec {
                init_config: serde_json::json!({ "base_url": "http://cfg" }),
                ..Default::default()
            },
        );
        let l = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            overrides,
            HashMap::new(),
            None,
            Vec::new(),
        );
        assert!(l.get_or_load("echo").is_some());
    }

    #[test]
    fn a_layer_with_no_usable_https_client_resolves_nothing() {
        // The machine cannot build an HTTPS client, so no script provider can
        // run. Reachable only by handing the failure in: reqwest will not fail
        // to build a client in any environment a test can arrange.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.rhai");
        std::fs::write(
            &path,
            "fn initialize(c) { #{} }\nfn inference(s, r) { #{ content: \"x\" } }",
        )
        .expect("write script");
        let layer = ScriptProviderLayer::with_executor(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
            Err(leviath_providers::provider::malformed_url_error()),
        );
        // The script is present and valid; only the client is missing.
        assert!(path.exists());
        assert!(
            layer.get_or_load("p").is_none(),
            "a layer with no client must not hand back a provider"
        );
    }
}
