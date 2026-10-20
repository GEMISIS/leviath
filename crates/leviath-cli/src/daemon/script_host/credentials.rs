//! Where a script host finds `[tool_credentials]` when a script asks for one.
//!
//! The daemon's environment is fixed when its process starts, so a key
//! exported later never reaches a tool. The config is not: the daemon reloads
//! `config.toml` when it changes. A [`CredentialSource::live`] reads the
//! config in force at the moment of each `env_var` call, so a credential
//! stored through the config API reaches the next call of a run that is
//! already going, without a restart.

use std::sync::Arc;

use crate::config::Config;
use crate::daemon::config_reload::ConfigReloader;

/// The lookup behind it: a name in, the secret out.
type Lookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// What a script host asks for a tool credential by name.
#[derive(Clone)]
pub(crate) struct CredentialSource(Arc<Lookup>);

impl CredentialSource {
    /// The config in force in this daemon, read on every call.
    pub(crate) fn live(reloader: Arc<ConfigReloader>) -> Self {
        Self(Arc::new(move |name| {
            reloader
                .in_force()
                .tool_credentials
                .get(name)
                .map(str::to_string)
        }))
    }

    /// One config, for a host that binds no live run.
    pub(crate) fn fixed(config: Arc<Config>) -> Self {
        Self(Arc::new(move |name| {
            config.tool_credentials.get(name).map(str::to_string)
        }))
    }

    /// No credentials at all.
    pub(crate) fn none() -> Self {
        Self(Arc::new(|_| None))
    }

    /// The secret stored under `name`, if there is one.
    pub(crate) fn get(&self, name: &str) -> Option<String> {
        (self.0)(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holding(name: &str, secret: &str) -> Config {
        let mut config = Config::default();
        config.tool_credentials.set(name, secret);
        config
    }

    #[test]
    fn none_holds_nothing() {
        assert_eq!(CredentialSource::none().get("BRAVE_API_KEY"), None);
    }

    #[test]
    fn fixed_answers_from_its_config() {
        let source = CredentialSource::fixed(Arc::new(holding("BRAVE_API_KEY", "sk-brave")));
        assert_eq!(source.get("BRAVE_API_KEY").as_deref(), Some("sk-brave"));
        assert_eq!(source.get("OTHER"), None);
    }

    /// The point of `live`: a save the reloader picks up is what the very
    /// next lookup sees, with the same source still in hand.
    #[test]
    fn live_follows_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let reloader = Arc::new(ConfigReloader::new(path.clone(), Config::default()));
        let source = CredentialSource::live(reloader.clone());
        assert_eq!(source.get("BRAVE_API_KEY"), None);

        std::fs::write(&path, "[tool_credentials]\nBRAVE_API_KEY = \"sk-new\"\n").unwrap();
        // A different mtime than the first write, which the reloader keys on.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        reloader.check();
        assert_eq!(source.get("BRAVE_API_KEY").as_deref(), Some("sk-new"));
    }
}
