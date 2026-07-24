//! Lazy, hot-reloading resolution of Rhai *script providers* (issue #101).
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
//! [`ProviderRegistry`]: crate::ProviderRegistry

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use leviath_providers::{ModelCapabilities, Provider, RateLimitConfig, RhaiProvider};

/// Per-provider configuration from `[model_providers.<name>]`. All fields are
/// optional overrides — a script activates by an agent referencing its name and
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

/// A cached, compiled script provider plus the source mtime it was built from.
struct Cached {
    mtime: SystemTime,
    provider: Arc<dyn Provider>,
}

/// Lazy, hot-reloading resolver for script providers.
pub struct ScriptProviderLayer {
    dir: PathBuf,
    overrides: HashMap<String, ScriptProviderSpec>,
    default_caps: HashMap<String, ModelCapabilities>,
    request_timeout_secs: Option<u64>,
    cache: Mutex<HashMap<String, Cached>>,
}

impl ScriptProviderLayer {
    /// Build a layer over `dir`, with per-provider `overrides`, global model
    /// capability overrides, and the global request timeout.
    pub fn new(
        dir: PathBuf,
        overrides: HashMap<String, ScriptProviderSpec>,
        default_caps: HashMap<String, ModelCapabilities>,
        request_timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            dir,
            overrides,
            default_caps,
            request_timeout_secs,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve `<name>` to its script path: an explicit `script` override
    /// (absolute path, or a stem/filename under the providers dir), else
    /// `<name>.rhai` in the providers dir.
    fn resolve_path(&self, name: &str) -> PathBuf {
        let stem = self
            .overrides
            .get(name)
            .and_then(|s| s.script.as_deref())
            .unwrap_or(name);
        let candidate = PathBuf::from(stem);
        if candidate.is_absolute() {
            return candidate;
        }
        if stem.ends_with(".rhai") {
            self.dir.join(stem)
        } else {
            self.dir.join(format!("{stem}.rhai"))
        }
    }

    /// Get (or lazily load / reload) the provider named `name`, or `None` when
    /// there is no such script or it fails to load.
    pub fn get_or_load(&self, name: &str) -> Option<Arc<dyn Provider>> {
        let path = self.resolve_path(name);
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();

        let mut cache = self.cache.lock().unwrap();
        match mtime {
            None => {
                // File gone (or unreadable): drop any stale entry, no provider.
                cache.remove(name);
                None
            }
            Some(mtime) => {
                if let Some(c) = cache.get(name)
                    && c.mtime == mtime
                {
                    return Some(c.provider.clone());
                }
                let spec = self.overrides.get(name);
                let init_config = spec
                    .map(|s| s.init_config.clone())
                    .unwrap_or_else(|| serde_json::json!({}));
                let rate_limit = spec.and_then(|s| s.rate_limit.clone());
                match RhaiProvider::from_script(
                    name.to_string(),
                    &path,
                    init_config,
                    self.default_caps.clone(),
                    rate_limit,
                    self.request_timeout_secs,
                ) {
                    Ok(p) => {
                        let provider: Arc<dyn Provider> = Arc::new(p);
                        cache.insert(
                            name.to_string(),
                            Cached {
                                mtime,
                                provider: provider.clone(),
                            },
                        );
                        Some(provider)
                    }
                    Err(e) => {
                        cache.remove(name);
                        tracing::warn!(provider = %name, error = %e, "script provider load failed");
                        None
                    }
                }
            }
        }
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

    fn layer(dir: PathBuf) -> ScriptProviderLayer {
        ScriptProviderLayer::new(dir, HashMap::new(), HashMap::new(), None)
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
        let l = ScriptProviderLayer::new(dir.path().to_path_buf(), overrides, HashMap::new(), None);
        assert_eq!(l.resolve_path("a"), dir.path().join("custom.rhai"));
        assert_eq!(l.resolve_path("b"), dir.path().join("custom.rhai"));
        assert_eq!(l.resolve_path("c"), abs);
        assert_eq!(l.resolve_path("z"), dir.path().join("z.rhai"));
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
        let l = ScriptProviderLayer::new(dir.path().to_path_buf(), overrides, HashMap::new(), None);
        assert!(l.get_or_load("echo").is_some());
    }
}
