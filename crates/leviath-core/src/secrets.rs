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

/// Compare two secrets without leaking their contents through timing.
///
/// Runs over the full length of both inputs rather than returning at the first
/// differing byte, so an attacker cannot recover a token one character at a time
/// by measuring how long a wrong guess takes. The *length* still leaks, which is
/// fine: these are fixed-shape tokens, and refusing early on a length mismatch
/// avoids indexing past the end.
///
/// Shared so every secret comparison in the workspace is the same one. The API
/// server had a correct implementation; the OAuth callback's `state` check used
/// `==` and was the only comparison that differed.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Render a secret for display, showing only its last four characters.
///
/// The **one** redaction policy for the workspace. There were two, and they
/// disagreed about which end of the value to keep: the HTTP logger showed the
/// last four, the setup wizard the first eight. Two answers to "how much of a
/// secret is safe to print" means neither is a policy — and a prefix is the
/// wrong half to keep, because API keys are structured at the front:
/// `sk-ant-a…`, `sk-proj-…`, `ghp_…` all identify the issuer and, for a short
/// token, a meaningful fraction of the value.
///
/// Counts **characters**, not bytes. `value.len() - 4` can land inside a
/// multi-byte character and panic (the shape of issue #115), and a 5-byte
/// 2-character value is longer than 4 *bytes*, so a byte-based length check
/// would print the whole thing behind four stars.
#[must_use]
pub fn redact(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    // Four visible characters out of five or fewer is most of the value.
    if chars.len() <= 8 {
        return "****".to_string();
    }
    format!(
        "****{}",
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

/// Whether a header's value must be redacted before it is logged.
///
/// A substring match rather than an exact-name list. The list version named
/// `authorization`, `x-api-key` and `api-key`, and therefore logged Gemini's
/// `x-goog-api-key` **in full** under `--features debug-http` — the one
/// provider whose header did not happen to be on it. A denylist of exact names
/// has to be complete to be correct, and this one was not; matching on the
/// shape of the name fails safe as new headers appear.
#[must_use]
pub fn is_secret_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["auth", "key", "token", "secret", "cookie", "credential"]
        .iter()
        .any(|hint| lower.contains(hint))
}

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

    /// The suffix is kept, not the prefix: API keys are structured at the front,
    /// so showing `sk-ant-a` names the issuer and, on a short token, exposes a
    /// meaningful fraction of the value.
    #[test]
    fn redact_keeps_only_a_short_suffix() {
        assert_eq!(redact("sk-ant-api-key-12345"), "****2345");
        assert!(!redact("sk-ant-api-key-12345").contains("sk-ant"));
        assert!(!redact("ghp_realgithubtoken").contains("ghp_"));
    }

    /// A short value is hidden entirely — four visible characters out of eight
    /// is most of it.
    #[test]
    fn redact_hides_short_values_completely() {
        for value in ["", "a", "abcd", "12345678"] {
            assert_eq!(redact(value), "****", "{value:?}");
        }
    }

    /// Issue #115: a byte-based cut lands inside a multi-byte character and
    /// panics, and a byte-length guard calls a short multi-byte value "long"
    /// and prints all of it.
    #[test]
    fn redact_counts_characters_not_bytes() {
        assert_eq!(redact("日本語日本語日本語"), "****語日本語");
        assert_eq!(redact("日本"), "****");
    }

    /// The exact-name list this replaces missed `x-goog-api-key`, so Gemini
    /// keys were logged in full under `--features debug-http`.
    #[test]
    fn secret_headers_are_matched_by_shape_not_an_exact_list() {
        for name in [
            "authorization",
            "Authorization",
            "x-api-key",
            "api-key",
            "x-goog-api-key",
            "proxy-authorization",
            "cookie",
            "set-cookie",
            "x-auth-token",
            "x-amz-security-token",
        ] {
            assert!(is_secret_header(name), "{name} must be redacted");
        }
    }

    #[test]
    fn ordinary_headers_are_not_redacted() {
        for name in [
            "content-type",
            "user-agent",
            "accept",
            "content-length",
            "anthropic-version",
        ] {
            assert!(!is_secret_header(name), "{name} should log verbatim");
        }
    }

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
