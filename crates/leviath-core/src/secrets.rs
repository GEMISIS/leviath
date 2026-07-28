//! Which environment variables agent-supplied code may see.
//!
//! Two different questions live here, and they want opposite shapes:
//!
//! 1. **What does a child process inherit?** ([`child_env_allowed`]) We are
//!    *choosing what to hand over*, so an allowlist is right. A denylist here has
//!    to enumerate every secret name in the ecosystem and loses the moment a new
//!    one appears — which is exactly what happened: the MCP spawner's substring
//!    denylist passed `AWS_SECRET_ACCESS_KEY` (it matches neither `API_SECRET`
//!    nor `SECRET_KEY`), `GITHUB_TOKEN`, `NPM_TOKEN`, `DATABASE_URL`, and
//!    Leviath's own `LEVIATH_API_TOKEN`.
//!
//! 2. **May a script read *this named* variable?** ([`is_sensitive_env_name`]) A
//!    script asks for one name it already knows. An allowlist cannot work — no
//!    fixed list covers every legitimate variable a provider script might read —
//!    so the rule inverts: anything that *looks like* a credential is refused
//!    unless the user allowlisted it, and everything else is fine.
//!
//! Neither is a substitute for the other, and both are shared rather than
//! reimplemented per call site so a gap gets fixed once.

/// Environment variables a spawned child (an MCP server, a shell tool) inherits.
///
/// Deliberately short. A child needs enough to find its interpreter and behave
/// like a terminal program; it does not need the ambient credentials of whoever
/// started the daemon. Anything else a server legitimately requires is declared
/// in its own `env` block in config, which is applied *after* this filter and so
/// always wins — that is the supported way to pass a server its token.
const CHILD_ENV_ALLOWLIST: &[&str] = &[
    // Finding and running programs.
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TEMP",
    "TMP",
    // Locale and terminal behaviour.
    "LANG",
    "LANGUAGE",
    "TERM",
    "TZ",
    "COLORTERM",
    "NO_COLOR",
    // Windows equivalents of the above.
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "PROGRAMDATA",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];

/// Whether a spawned child process may inherit `name`.
///
/// Case-insensitive, because Windows environment variables are.
pub fn child_env_allowed(name: &str) -> bool {
    CHILD_ENV_ALLOWLIST
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Substrings that make a variable name look like it holds a credential.
///
/// Matched case-insensitively anywhere in the name, so `AWS_SECRET_ACCESS_KEY`,
/// `GH_TOKEN`, `npm_config_//registry:_authToken`, and `DATABASE_PASSWORD` all
/// hit. Broad on purpose: a false positive costs the user one allowlist entry,
/// a false negative costs them the credential.
const SECRET_NAME_HINTS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "APIKEY",
    "API_KEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "SESSION",
    "COOKIE",
    "AUTH",
    "BEARER",
    "SIGNATURE",
    "SIGNING",
];

/// Exact names that are sensitive without matching any of [`SECRET_NAME_HINTS`].
const SECRET_NAME_EXACT: &[&str] = &[
    // Connection strings routinely embed a username and password.
    "DATABASE_URL",
    "DATABASE_DSN",
    "REDIS_URL",
    "MONGO_URL",
    "MONGODB_URI",
    "AMQP_URL",
    // Points at an agent socket that can sign on the user's behalf.
    "SSH_AUTH_SOCK",
];

/// Prefixes whose whole namespace is treated as sensitive.
const SECRET_NAME_PREFIXES: &[&str] = &[
    // Leviath's own: `LEVIATH_API_TOKEN` authenticates the agent-spawning API,
    // and `LEVIATH_CONFIG_PATH` / `LEVIATH_HOME` redirect where secrets are read
    // from. None of it is a script's business.
    "LEVIATH_", // Cloud SDK credential namespaces.
    "AWS_", "AZURE_", "GOOGLE_", "GCP_",
];

/// Whether `name` looks like it holds a credential.
///
/// Used to decide whether an explicit `env_var("NAME")` read from an agent's
/// Rhai script is refused. The check is on the *name*, never the value: a value
/// test would have to read the secret to decide whether reading it was allowed.
pub fn is_sensitive_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_HINTS.iter().any(|h| upper.contains(h))
        || SECRET_NAME_EXACT.iter().any(|e| upper == *e)
        || SECRET_NAME_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Whether a script may read `name`, given the user's `[security] allow_env_vars`.
///
/// Non-credential names pass freely — a script reading `PATH`, `TZ`, or its own
/// app's config variable is ordinary. A credential-shaped name passes only if the
/// user listed it, which is them saying "yes, this agent is meant to have that".
/// Matching the allowlist is case-insensitive and exact; no wildcards, because
/// `allow_env_vars = ["*"]` would read as a shortcut rather than the decision it
/// actually is.
pub fn script_env_allowed(name: &str, allowlist: &[String]) -> bool {
    !is_sensitive_env_name(name)
        || allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every one of these slipped through the substring denylist this replaces.
    #[test]
    fn catches_what_the_old_denylist_missed() {
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "NPM_TOKEN",
            "HF_TOKEN",
            "SLACK_TOKEN",
            "DATABASE_URL",
            "LEVIATH_API_TOKEN",
            "SSH_AUTH_SOCK",
        ] {
            assert!(is_sensitive_env_name(name), "{name} should be sensitive");
            assert!(!child_env_allowed(name), "{name} must not reach a child");
        }
    }

    #[test]
    fn catches_the_provider_keys() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            assert!(is_sensitive_env_name(name), "{name}");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(is_sensitive_env_name("github_token"));
        assert!(is_sensitive_env_name("MyApp_Password"));
        assert!(child_env_allowed("path"));
        assert!(child_env_allowed("Path"));
    }

    #[test]
    fn ordinary_names_are_not_sensitive() {
        for name in ["PATH", "HOME", "TZ", "TERM", "EDITOR", "MY_APP_REGION"] {
            assert!(!is_sensitive_env_name(name), "{name}");
        }
    }

    #[test]
    fn child_allowlist_is_the_short_list_not_the_environment() {
        assert!(child_env_allowed("PATH"));
        assert!(child_env_allowed("LANG"));
        // Not sensitive, but still not a child's business by default — the point
        // of an allowlist is that unknown names are excluded, not just secret
        // ones. A server that needs it declares it in its own `env` block.
        assert!(!child_env_allowed("MY_APP_REGION"));
        assert!(!child_env_allowed("EDITOR"));
    }

    #[test]
    fn script_reads_pass_unless_credential_shaped() {
        let none: &[String] = &[];
        assert!(script_env_allowed("PATH", none));
        assert!(script_env_allowed("MY_APP_REGION", none));
        assert!(!script_env_allowed("ANTHROPIC_API_KEY", none));
    }

    #[test]
    fn allowlisting_a_credential_permits_exactly_that_one() {
        let allow = vec!["MY_PROVIDER_KEY".to_string()];
        assert!(script_env_allowed("MY_PROVIDER_KEY", &allow));
        assert!(script_env_allowed("my_provider_key", &allow), "case");
        assert!(!script_env_allowed("ANTHROPIC_API_KEY", &allow));
        // No wildcard support: `*` is a literal name, not "everything".
        assert!(!script_env_allowed("ANTHROPIC_API_KEY", &["*".to_string()]));
    }
}
