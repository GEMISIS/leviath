//! `[tool_credentials]`: secrets a script tool reads by name.
//!
//! A script tool asks for a credential with `env_var("NAME")`. The name is
//! looked up here first, and only then in the daemon's environment, so a key
//! stored through the config API reaches the tool on its next call. The
//! daemon's environment is fixed when it starts, which is why a key exported
//! after that never could.
//!
//! Storing a name here is the grant. A credential-shaped name read from the
//! environment still has to be listed in `[security] allow_env_vars`, but one
//! the operator put in this table was put there for tools to read.
//!
//! The values follow `[security] credential_store` like the provider keys. In
//! `file` mode they sit in `config.toml`, written `0600`. In `keychain` mode
//! each one is filed under its own account and the file keeps only the name,
//! with an empty value, because the OS stores cannot list what they hold.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The longest name a credential may have.
const MAX_NAME_LEN: usize = 128;

/// Every tool credential the config holds, by name.
///
/// An empty value is a name with no secret behind it: what a keychain-mode
/// file holds before the store fills it in. [`ToolCredentials::get`] treats
/// it as absent; [`ToolCredentials::names`] still lists the name.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCredentials(BTreeMap<String, String>);

/// Names only. `Config` derives `Debug`, and a debug print of it is the kind
/// of thing that ends up in a log.
impl std::fmt::Debug for ToolCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl ToolCredentials {
    /// The secret stored under `name`, matched exactly.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// Every name the table holds, sorted, including one still waiting on
    /// the store: in keychain mode a name is listed from the file even when
    /// the store has nothing for it, so a client can see it and remove it.
    pub fn names(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }

    /// Store `secret` under `name`, replacing what was there.
    pub(crate) fn set(&mut self, name: &str, secret: &str) {
        self.0.insert(name.to_string(), secret.to_string());
    }

    /// Take `name` out. Says whether it was there.
    pub(crate) fn remove(&mut self, name: &str) -> bool {
        self.0.remove(name).is_some()
    }

    /// Every entry, including the names still waiting on the store.
    pub(super) fn entries_mut(&mut self) -> impl Iterator<Item = (&String, &mut String)> {
        self.0.iter_mut()
    }

    /// The names whose secret has not been filled in.
    pub(super) fn unfilled(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, value)| value.is_empty())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every `(name, secret)` pair with a secret.
    pub(super) fn secrets(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// The credential store account a tool credential is filed under.
///
/// Its own prefix, so a tool credential can never collide with a provider key
/// (`provider/...`) or an MCP grant (`mcp/...`).
pub(crate) fn account(name: &str) -> String {
    format!("tool/{name}")
}

/// Check that `name` can be a credential's name: what an environment variable
/// may be called, since that is the name a script asks for.
///
/// The refusal names the name and never the value.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let starts_well = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if starts_well && rest_ok && name.len() <= MAX_NAME_LEN {
        return Ok(());
    }
    Err(format!(
        "'{name}' is not a tool credential name: use letters, digits and underscores, starting \
         with a letter or an underscore, at most {MAX_NAME_LEN} characters"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(entries: &[(&str, &str)]) -> ToolCredentials {
        let mut credentials = ToolCredentials::default();
        for (name, value) in entries {
            credentials.set(name, value);
        }
        credentials
    }

    /// An empty value is a name the store has not filled, and reads as no
    /// credential at all.
    #[test]
    fn an_empty_value_reads_as_absent() {
        let credentials = with(&[("BRAVE_API_KEY", "sk-brave"), ("WAITING", "")]);
        assert_eq!(credentials.get("BRAVE_API_KEY"), Some("sk-brave"));
        assert_eq!(credentials.get("WAITING"), None);
        assert_eq!(credentials.get("brave_api_key"), None, "matched exactly");
        assert_eq!(
            credentials.names(),
            vec!["BRAVE_API_KEY".to_string(), "WAITING".to_string()]
        );
        assert_eq!(credentials.unfilled(), vec!["WAITING".to_string()]);
        assert_eq!(
            credentials.secrets().collect::<Vec<_>>(),
            vec![("BRAVE_API_KEY", "sk-brave")]
        );
    }

    #[test]
    fn remove_says_whether_the_name_was_there() {
        let mut credentials = with(&[("A", "one")]);
        assert!(credentials.remove("A"));
        assert!(!credentials.remove("A"));
    }

    /// The debug form is what a stray `{:?}` of the whole config prints.
    #[test]
    fn debug_prints_names_and_never_values() {
        let printed = format!("{:?}", with(&[("BRAVE_API_KEY", "sk-brave-secret")]));
        assert!(printed.contains("BRAVE_API_KEY"), "{printed}");
        assert!(!printed.contains("sk-brave-secret"), "{printed}");
    }

    #[test]
    fn the_table_is_a_plain_toml_table() {
        let parsed: ToolCredentials = toml::from_str("BRAVE_API_KEY = \"sk\"\n").unwrap();
        assert_eq!(parsed.get("BRAVE_API_KEY"), Some("sk"));
        assert_eq!(
            toml::to_string(&parsed).unwrap(),
            "BRAVE_API_KEY = \"sk\"\n"
        );
    }

    #[test]
    fn accounts_have_their_own_prefix() {
        assert_eq!(account("BRAVE_API_KEY"), "tool/BRAVE_API_KEY");
    }

    #[test]
    fn names_are_what_an_environment_variable_may_be_called() {
        for good in ["BRAVE_API_KEY", "_x", "a1"] {
            assert_eq!(check_name(good), Ok(()), "{good}");
        }
        let long = "A".repeat(MAX_NAME_LEN + 1);
        for bad in ["", "1ABC", "HAS SPACE", "DASH-ED", "tool/x", long.as_str()] {
            let refusal = check_name(bad).unwrap_err();
            assert!(refusal.contains("not a tool credential name"), "{refusal}");
        }
    }
}
