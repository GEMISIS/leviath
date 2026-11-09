//! The user's provider credentials, read the way `lev` reads them, for a host
//! that embeds the runtime and builds its own [`ProviderRegistry`](crate::ProviderRegistry).
//!
//! [`load_user_creds`] reads `config.toml`, fills every key the file leaves
//! unset from the environment and then, under `[security] credential_store =
//! "keychain"`, from the OS credential store: file, then environment, then
//! keychain, the CLI's precedence. What comes back is the
//! [`ProviderCreds`] list [`build_provider_registry`](crate::build_provider_registry)
//! takes, the [`ModelDefaults`] a stage resolves against, and where each key
//! came from.
//!
//! Every input that reaches outside the process is the caller's to grant on
//! [`UserCredsSource`]: which data directory, whether the environment counts,
//! and whether the OS store may be opened. [`UserCredsSource::system`] grants
//! all three for the real install; a sandbox passes its own directory and
//! leaves the rest out.
//!
//! # Scope
//!
//! The built-in providers only: the API-keyed ones in [`KEYED_PROVIDERS`],
//! Ollama, and the Codex and Grok sign-ins. `[model_providers]` entries
//! (OpenAI-compatible endpoints and Rhai script providers) are not read, nor
//! the retention words they carry, and no `.env` file is loaded: a host that
//! wants either reads them itself. Unknown keys in the file are ignored, since
//! the CLI's configuration is a superset of what this reads.

mod creds;
mod model_provider;
mod provider_config;
pub mod renamed;
mod settings;

pub use model_provider::{ModelProviderConfig, ModelProviderKind};
pub use provider_config::ProviderConfig;
pub use settings::{SecuritySettings, UserSettings};

use crate::pipeline::ModelDefaults;
use crate::provider_creds::ProviderCreds;
use leviath_core::{CredentialStore, CredentialStoreKind};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// One API-keyed provider and the environment variables that stand in for
/// what the config file leaves unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyedProvider {
    /// The provider's registry name, which is also the credential store
    /// account it is filed under (`provider/<name>`).
    pub provider: &'static str,
    /// The variable read when the file leaves the key unset.
    pub key_env: &'static str,
    /// The variable read when the file leaves the provider's host unset, if
    /// it has one.
    pub base_url_env: Option<&'static str>,
}

/// Every API-keyed provider, in the order they are registered.
///
/// The one table of them: the CLI's config loader, its credential-store
/// overlay, `lev auth` and [`load_user_creds`] all look providers up here by
/// name, through [`keyed_provider`], so an entry added or moved cannot shift
/// one provider's variable onto another.
pub const KEYED_PROVIDERS: &[KeyedProvider] = &[
    KeyedProvider {
        provider: "anthropic",
        key_env: "ANTHROPIC_API_KEY",
        base_url_env: Some("ANTHROPIC_BASE_URL"),
    },
    KeyedProvider {
        provider: "openai",
        key_env: "OPENAI_API_KEY",
        base_url_env: Some("OPENAI_BASE_URL"),
    },
    KeyedProvider {
        provider: "google",
        key_env: "GOOGLE_API_KEY",
        base_url_env: Some("GOOGLE_BASE_URL"),
    },
    // The one key at the top level of the file rather than under
    // `[providers]`.
    KeyedProvider {
        provider: "openrouter",
        key_env: "OPENROUTER_API_KEY",
        base_url_env: Some("OPENROUTER_BASE_URL"),
    },
    KeyedProvider {
        provider: "meshy",
        key_env: "MESHY_API_KEY",
        base_url_env: Some("MESHY_BASE_URL"),
    },
    // The key variable AWS's own tooling reads.
    KeyedProvider {
        provider: leviath_providers::bedrock::PROVIDER_NAME,
        key_env: leviath_providers::bedrock::KEY_ENV,
        base_url_env: Some("BEDROCK_BASE_URL"),
    },
    KeyedProvider {
        provider: leviath_providers::xai::PROVIDER_NAME,
        key_env: "XAI_API_KEY",
        base_url_env: Some("XAI_BASE_URL"),
    },
    // Not Meta's own `MODEL_API_KEY`: a name that generic could belong to
    // anything on the machine.
    KeyedProvider {
        provider: leviath_providers::meta::PROVIDER_NAME,
        key_env: "META_AI_API_KEY",
        base_url_env: Some("META_AI_BASE_URL"),
    },
];

/// The entry in [`KEYED_PROVIDERS`] for `provider`, or `None` when it is not
/// an API-keyed built-in.
pub fn keyed_provider(provider: &str) -> Option<&'static KeyedProvider> {
    KEYED_PROVIDERS.iter().find(|k| k.provider == provider)
}

/// The file the provider sign-in grants are kept in, under the data
/// directory. The name `ProviderAuthStore::default_path` uses.
const GRANTS_FILE: &str = "provider-auth.json";
/// The shared model-capability cache, under the data directory. The name
/// [`leviath_core::paths::capability_cache_path`] uses.
const CAPABILITY_CACHE_FILE: &str = "model_capabilities.json";

/// Reads the environment: a variable's value, or `None` when it is unset.
pub type EnvReader<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Opens the OS credential store, or says why it cannot.
pub type KeychainOpener<'a> = &'a dyn Fn() -> Result<Arc<dyn CredentialStore>, String>;

/// Where [`load_user_creds`] reads from, and what it may touch.
pub struct UserCredsSource<'a> {
    /// Leviath's data directory, `<home>/.leviath` for the real install. The
    /// sign-in grants and the capability cache are found under it, and the
    /// config too unless [`config_path`](Self::config_path) says otherwise.
    pub data_dir: PathBuf,
    /// The config file. `None` reads `data_dir/config.toml`.
    pub config_path: Option<PathBuf>,
    /// The environment the fallbacks read. `None` ignores the environment
    /// entirely, so only the file and the keychain supply keys.
    pub env: Option<EnvReader<'a>>,
    /// How to open the OS credential store, used only when the config says
    /// `credential_store = "keychain"` and a key is still missing after the
    /// file and the environment. `None` never touches the OS store; a config
    /// that asks for it then loads with a warning instead.
    pub keychain: Option<KeychainOpener<'a>>,
}

/// The real environment, for [`UserCredsSource::system`].
fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The real OS credential store, for [`UserCredsSource::system`]: probed once,
/// so a machine with no usable store says so here rather than at every key.
fn system_keychain() -> Result<Arc<dyn CredentialStore>, String> {
    leviath_sys::keychain::probe(leviath_core::credentials::SERVICE).map(|()| {
        leviath_providers::oauth::store::store_for(CredentialStoreKind::Keychain)
            .expect("the keychain backend always has a store")
    })
}

impl UserCredsSource<'static> {
    /// The real install, read as `lev` reads it: [`leviath_core::data_dir`]
    /// (which honours `LEVIATH_HOME`), `LEVIATH_CONFIG_PATH` when it is set,
    /// the process environment, and the OS credential store.
    ///
    /// One difference from `lev`: no `.env` file is loaded. `lev` reads
    /// `./.env` into its environment when `load_dotenv = true` or
    /// `LEVIATH_LOAD_DOTENV=1` asks it to; a host that wants the same keys
    /// loads that file into its own process environment before calling this
    /// (skipping the names `leviath_core::dotenv_var_allowed` refuses), or
    /// passes an [`env`](Self::env) reader that consults it.
    ///
    /// `None` when no home directory can be resolved.
    pub fn system() -> Option<Self> {
        Self::system_at(
            leviath_core::data_dir(),
            std::env::var_os("LEVIATH_CONFIG_PATH").map(PathBuf::from),
        )
    }

    /// [`system`](Self::system), with the two paths it reads from the
    /// environment handed in.
    fn system_at(data_dir: Option<PathBuf>, config_path: Option<PathBuf>) -> Option<Self> {
        Some(Self {
            data_dir: data_dir?,
            config_path,
            env: Some(&process_env),
            keychain: Some(&system_keychain),
        })
    }
}

/// Where a provider's API key came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOrigin {
    /// `config.toml`.
    File,
    /// The environment variable of this name.
    Env(String),
    /// The OS credential store.
    Keychain,
}

/// What [`load_user_creds`] found.
///
/// `Debug` prints no secret: [`ProviderCreds`]'s own redacts its key.
#[derive(Debug, Clone)]
pub struct UserCreds {
    /// The providers to register, as
    /// [`build_provider_registry`](crate::build_provider_registry) takes them.
    pub creds: Vec<ProviderCreds>,
    /// The default provider, provider order, fallback chain, override and
    /// fallback models, and retention settings a stage resolves against.
    pub defaults: ModelDefaults,
    /// Where the config says secrets are kept.
    pub credential_store: CredentialStoreKind,
    /// The provider sign-in grants file, `data_dir/provider-auth.json`. The
    /// Codex and Grok entries in [`creds`](Self::creds) already carry it.
    pub grants_path: PathBuf,
    /// The shared model-capability cache, `data_dir/model_capabilities.json`.
    pub capability_cache: PathBuf,
    /// Where each registered API-keyed provider's key came from, by provider.
    pub origins: BTreeMap<String, KeyOrigin>,
    /// What went wrong without stopping the load: a keychain that was asked
    /// for and could not be opened, a key variable set but empty, a malformed
    /// fallback entry.
    pub warnings: Vec<String>,
}

/// Why [`load_user_creds`] could not read the config.
#[derive(Debug)]
pub enum UserCredsError {
    /// The file exists and could not be read.
    Read {
        /// The config file.
        path: PathBuf,
        /// What reading it said.
        source: std::io::Error,
    },
    /// The file is not a config the settings can be read from.
    Parse {
        /// The config file.
        path: PathBuf,
        /// What parsing it said.
        source: toml::de::Error,
    },
}

impl std::fmt::Display for UserCredsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "could not parse {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for UserCredsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

/// Read the user's provider credentials from `source`.
///
/// A config file that does not exist is the defaults, as it is for `lev`;
/// one that exists and cannot be read or parsed is an error. A keychain that
/// cannot be opened is a warning, never an error: keys from the file and the
/// environment still load. The keychain is opened only when a key is still
/// missing after the file and the environment.
pub fn load_user_creds(source: &UserCredsSource<'_>) -> Result<UserCreds, UserCredsError> {
    let path = source
        .config_path
        .clone()
        .unwrap_or_else(|| source.data_dir.join("config.toml"));
    let mut warnings = Vec::new();
    let mut settings = read_settings(path, &mut warnings)?;

    let mut origins = BTreeMap::new();
    for keyed in KEYED_PROVIDERS {
        if (settings::slots(keyed.provider).key_ref)(&settings).is_some() {
            origins.insert(keyed.provider.to_string(), KeyOrigin::File);
        }
    }
    if let Some(env) = source.env {
        settings.fill_from_env(env, &mut origins, &mut warnings);
    }
    let credential_store = settings.security.credential_store;
    let missing = settings.missing_keys();
    if credential_store == CredentialStoreKind::Keychain && !missing.is_empty() {
        match source.keychain.map(|open| open()) {
            Some(Ok(store)) => settings.fill_from_store(store.as_ref(), &missing, &mut origins),
            Some(Err(e)) => warnings.push(format!(
                "`[security] credential_store = \"keychain\"` is set, but {e}. Using keys from \
                 the config file and environment."
            )),
            None => warnings.push(
                "`[security] credential_store = \"keychain\"` is set, but this source does not \
                 read the OS credential store, so keys kept there are not loaded."
                    .to_string(),
            ),
        }
    }

    let grants_path = source.data_dir.join(GRANTS_FILE);
    let creds = creds::provider_creds(&settings, &grants_path);
    // Only providers that made it into the registry list: a blank key is
    // not a key, wherever it came from.
    origins.retain(|provider, _| creds.iter().any(|c| &c.name == provider));
    let defaults = creds::model_defaults(&settings, &mut warnings);
    Ok(UserCreds {
        creds,
        defaults,
        credential_store,
        grants_path,
        capability_cache: source.data_dir.join(CAPABILITY_CACHE_FILE),
        origins,
        warnings,
    })
}

/// The settings in the file at `path`, or the defaults when there is none.
///
/// "None" is decided the way the CLI decides it, by [`Path::exists`], so
/// `lev` and an embedder agree on every install: a path that does not
/// resolve is the defaults, and a file that is there but will not read is
/// an error.
///
/// A key that changed name is respelled in the text before it is parsed, by
/// the rewrite `lev` runs, and each one this loader reads is named in
/// `warnings`. Done on the text, so a parse error still points at its line.
///
/// [`Path::exists`]: std::path::Path::exists
fn read_settings(
    path: PathBuf,
    warnings: &mut Vec<String>,
) -> Result<UserSettings, UserCredsError> {
    if !path.exists() {
        return Ok(UserSettings::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|source| UserCredsError::Read {
        path: path.clone(),
        source,
    })?;
    let (text, renames) = renamed::rename_in_text(&text);
    let settings =
        toml::from_str(&text).map_err(|source| UserCredsError::Parse { path, source })?;
    warnings.extend(
        renames
            .iter()
            .filter(|renamed| READ_TABLES.contains(&renamed.key.section))
            .map(renamed::notice),
    );
    Ok(settings)
}

/// The tables [`UserSettings`] reads keys from: the top of the file, and
/// these. A renamed key anywhere else (`[sandbox]`, say) is not this
/// loader's to report.
const READ_TABLES: &[Option<&str>] = &[None, Some("providers"), Some("security")];

#[cfg(test)]
mod tests;
