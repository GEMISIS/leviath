//! Which environment variables agent-supplied code may see.
//!
//! Three different questions live here, and the first two want opposite shapes:
//!
//! 1. **What does a child process inherit?** ([`child_env_allowed`]) We are
//!    *choosing what to hand over*, so an allowlist is right. A denylist here has
//!    to enumerate every secret name in the ecosystem and loses the moment a new
//!    one appears - which is exactly what happened: the MCP spawner's substring
//!    denylist passed `AWS_SECRET_ACCESS_KEY` (it matches neither `API_SECRET`
//!    nor `SECRET_KEY`), `GITHUB_TOKEN`, `NPM_TOKEN`, `DATABASE_URL`, and
//!    Leviath's own `LEVIATH_API_TOKEN`.
//!
//! 2. **May a script read *this named* variable?** ([`is_sensitive_env_name`]) A
//!    script asks for one name it already knows. An allowlist cannot work - no
//!    fixed list covers every legitimate variable a provider script might read -
//!    so the rule inverts: anything that *looks like* a credential is refused
//!    unless the user allowlisted it, and everything else is fine.
//!
//! 3. **May a `.env` in the working directory set this variable?**
//!    ([`dotenv_var_allowed`]) Neither shape above fits. The whole point of
//!    loading a `.env` is to pick up credentials, so the credential test from
//!    (2) would refuse exactly what the feature exists for; and the names worth
//!    refusing are not secrets at all but the handful that *steer the process* -
//!    where config is read from, what gets executed. So this one is a small,
//!    closed denylist of process-steering names, and everything else passes.
//!
//! None is a substitute for the others, and all are shared rather than
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
/// secret is safe to print" means neither is a policy - and a prefix is the
/// wrong half to keep, because API keys are structured at the front:
/// `sk-ant-a…`, `sk-proj-…`, `ghp_…` all identify the issuer and, for a short
/// token, a meaningful fraction of the value.
///
/// Counts **characters**, not bytes. `value.len() - 4` can land inside a
/// multi-byte character and panic, and a 5-byte
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
/// `x-goog-api-key` **in full** under `--features debug-http` - the one
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
/// always wins - that is the supported way to pass a server its token.
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
    // Bare `KEY`, which subsumes the three above and catches everything they
    // missed: `OPENAI_KEY`, `ENCRYPTION_KEY`, `MASTER_KEY`, `DEPLOY_KEY`. The
    // longer forms stay for documentation value. A variable whose name merely
    // contains "key" and is not a secret (`KEYBOARD_LAYOUT`, `KEYCHAIN_PATH`)
    // costs its owner one `allow_env_vars` entry, which is the trade this list
    // is meant to make.
    "KEY",
    // A personal access token, which is what `_PAT` conventionally means.
    "_PAT",
    "SESSION",
    "COOKIE",
    "AUTH",
    "BEARER",
    "SIGNATURE",
    "SIGNING",
    // Sentry DSNs embed a key; `.netrc` and kubeconfigs are credential files.
    "DSN",
    "NETRC",
    "KUBECONFIG",
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

/// Exact names a `./.env` may not set, because each one steers the process
/// rather than configuring it.
const DOTENV_DENY_EXACT: &[&str] = &[
    // Split and spawned as a program by the editor flow.
    "EDITOR", "VISUAL",
    // Decide what a shell tool, an MCP server command, or a seed command
    // resolves to.
    "PATH", "SHELL",
];

/// Prefixes a `./.env` may not set.
const DOTENV_DENY_PREFIXES: &[&str] = &[
    // `LEVIATH_CONFIG_PATH` and `LEVIATH_HOME` relocate where config, agents and
    // scripts are read from, so setting either replaces the whole trust base:
    // `[mcp_servers]` commands, `[tool_permissions]`, `[sandbox]`, provider
    // `base_url`. `LEVIATH_API_TOKEN` sets a known credential on the
    // agent-spawning API. None of it is a repository's business.
    "LEVIATH_", // Injected into every child the dynamic linker starts.
    "LD_", "DYLD_",
];

/// Whether a `./.env` in the working directory may set `name`.
///
/// `lev` is designed to be run inside cloned repositories, so `./.env` is
/// attacker-authored content on any repository the user did not write. dotenvy
/// does not override an already-set variable, which protects `PATH` and `HOME`
/// in practice - but not a variable that is normally *unset*, and the ones that
/// matter most here are exactly those.
///
/// Deliberately **not** built on [`is_sensitive_env_name`]: that would refuse
/// `ANTHROPIC_API_KEY`, which is the entire legitimate purpose of loading a
/// `.env`. Credentials are what this feature is for; the denylist is only the
/// names that decide where config comes from and what gets executed.
///
/// `OLLAMA_HOST` and `*_BASE_URL`-shaped names are a deliberate edge, left
/// allowed: pointing your own inference endpoint from a repository's `.env` is
/// something people do on purpose, and a repository that can already choose your
/// model can already choose your outputs.
#[must_use]
pub fn dotenv_var_allowed(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    !DOTENV_DENY_EXACT.contains(&upper.as_str())
        && !DOTENV_DENY_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// How much of the daemon's environment a shell tool inherits.
///
/// A fourth question again, and a fourth shape. A shell tool is a child we hand
/// over to, like an MCP server - but unlike one, it must keep behaving like the
/// user's own shell, so [`child_env_allowed`]'s 28-name allowlist is wrong here:
/// it would strip `CARGO_HOME`, `JAVA_HOME`, `NVM_DIR`, `VIRTUAL_ENV`, `GOPATH`
/// and break every real toolchain. The name-shape denylist is the right
/// instrument, and the only real question is how far it reaches.
///
/// Be honest about what this buys. With `cat` and `grep` on the default safe
/// list, a granted shell can read `~/.leviath/config.toml` and find the provider
/// key anyway. This is defence in depth against *accidental* leakage - an env
/// dump in tool output, a `printenv` in a log, a subprocess that phones home -
/// and it closes the seed-command case, where nothing was ever approved. It is
/// not a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellEnvMode {
    /// Withhold credential-shaped names, but hand over `SSH_AUTH_SOCK`.
    ///
    /// The carve-out is deliberate and is why this can be the default: the
    /// agent socket is on the credential-name list, and withholding it breaks
    /// `git push` over agent keys, which is one of the most ordinary things an
    /// agent does in a shell.
    #[default]
    Filtered,
    /// The full name-shape denylist, `SSH_AUTH_SOCK` included - and with it
    /// `AWS_PROFILE`, `AWS_REGION`, `KUBECONFIG`, `NETRC`. Breaks `git push`,
    /// `aws` and `kubectl` in a shell tool until those names are listed in
    /// `[security] allow_env_vars`.
    Strict,
    /// Ignore the shape heuristic entirely: withhold exactly what
    /// `[security] shell_env_withhold` names, and nothing else. For an
    /// environment whose variable names the heuristic reads wrong in either
    /// direction.
    Custom,
    /// Hand the whole environment over, as before this setting existed.
    Inherit,
}

/// The variables in `names` a shell tool must not inherit.
///
/// Returns names rather than mutating a `Command`, so the decision is a pure
/// function testable without spawning anything - which is what keeps its tests
/// deterministic on Windows.
///
/// `allow_env_vars` wins under every mode. It already means "yes, this agent is
/// meant to have that" for a Rhai `env_var` read, and one list with one meaning
/// is worth more than a second list that means almost the same thing.
pub fn withheld_child_vars<'a>(
    names: impl Iterator<Item = &'a str>,
    mode: ShellEnvMode,
    allow_env_vars: &[String],
    custom_withhold: &[String],
) -> Vec<String> {
    let listed = |list: &[String], name: &str| list.iter().any(|e| e.eq_ignore_ascii_case(name));
    names
        .filter(|name| match mode {
            ShellEnvMode::Inherit => false,
            ShellEnvMode::Custom => listed(custom_withhold, name) && !listed(allow_env_vars, name),
            ShellEnvMode::Filtered if name.eq_ignore_ascii_case("SSH_AUTH_SOCK") => false,
            ShellEnvMode::Filtered | ShellEnvMode::Strict => {
                !script_env_allowed(name, allow_env_vars)
            }
        })
        .map(str::to_string)
        .collect()
}

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
/// Non-credential names pass freely - a script reading `PATH`, `TZ`, or its own
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

    /// Runs over the full length of both inputs rather than returning at the
    /// first differing byte, so a wrong token cannot be recovered one character
    /// at a time. The length still leaks, which is fine for fixed-shape tokens.
    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(constant_time_eq("", ""));
        assert!(!constant_time_eq("secret", "secreu"));
        // Differing at the very first byte and at the very last must both be
        // false - the loop does not short-circuit either way.
        assert!(!constant_time_eq("Xecret", "secret"));
        assert!(!constant_time_eq("secreX", "secret"));
        // Length mismatch is refused before indexing.
        assert!(!constant_time_eq("secret", "secretx"));
        assert!(!constant_time_eq("secretx", "secret"));
    }

    /// The suffix is kept, not the prefix: API keys are structured at the front,
    /// so showing `sk-ant-a` names the issuer and, on a short token, exposes a
    /// meaningful fraction of the value.
    #[test]
    fn redact_keeps_only_a_short_suffix() {
        assert_eq!(redact("sk-ant-api-key-12345"), "****2345");
        assert!(!redact("sk-ant-api-key-12345").contains("sk-ant"));
        assert!(!redact("ghp_realgithubtoken").contains("ghp_"));
    }

    /// A short value is hidden entirely - four visible characters out of eight
    /// is most of it.
    #[test]
    fn redact_hides_short_values_completely() {
        for value in ["", "a", "abcd", "12345678"] {
            assert_eq!(redact(value), "****", "{value:?}");
        }
    }

    /// A byte-based cut lands inside a multi-byte character and panics, and a
    /// byte-length guard calls a short multi-byte value "long" and prints all
    /// of it.
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

    // ─── what a shell tool inherits ───────────────────────────────────────

    /// A sample environment spanning what a real daemon holds: credentials it
    /// must not hand over, and the toolchain variables every real command needs.
    const SAMPLE_ENV: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "LEVIATH_API_TOKEN",
        "DATABASE_URL",
        "SSH_AUTH_SOCK",
        "KUBECONFIG",
        "PATH",
        "HOME",
        "CARGO_HOME",
        "JAVA_HOME",
        "VIRTUAL_ENV",
        "NVM_DIR",
        "GOPATH",
        "DOCKER_HOST",
        "TERM",
    ];

    fn withheld(mode: ShellEnvMode, allow: &[&str], custom: &[&str]) -> Vec<String> {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        let custom: Vec<String> = custom.iter().map(|s| s.to_string()).collect();
        withheld_child_vars(SAMPLE_ENV.iter().copied(), mode, &allow, &custom)
    }

    /// The default. Credentials are withheld, every toolchain variable passes,
    /// and `SSH_AUTH_SOCK` is the deliberate carve-out that lets this be the
    /// default at all - without it `git push` over agent keys breaks in a shell
    /// tool, which is one of the most ordinary things an agent does.
    #[test]
    fn filtered_withholds_credentials_and_keeps_toolchains() {
        let out = withheld(ShellEnvMode::Filtered, &[], &[]);
        for secret in [
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "LEVIATH_API_TOKEN",
            "DATABASE_URL",
        ] {
            assert!(out.iter().any(|n| n == secret), "{secret} must be withheld");
        }
        for kept in [
            "SSH_AUTH_SOCK",
            "PATH",
            "HOME",
            "CARGO_HOME",
            "JAVA_HOME",
            "VIRTUAL_ENV",
            "NVM_DIR",
            "GOPATH",
            "DOCKER_HOST",
            "TERM",
        ] {
            assert!(!out.iter().any(|n| n == kept), "{kept} must pass through");
        }
    }

    /// `strict` drops the carve-out, which is the whole difference, and takes
    /// the credential-file names with it.
    #[test]
    fn strict_also_withholds_the_agent_socket() {
        let out = withheld(ShellEnvMode::Strict, &[], &[]);
        assert!(out.iter().any(|n| n == "SSH_AUTH_SOCK"));
        assert!(out.iter().any(|n| n == "KUBECONFIG"));
        // Still not a toolchain-breaker.
        assert!(!out.iter().any(|n| n == "PATH"));
        assert!(!out.iter().any(|n| n == "CARGO_HOME"));
    }

    /// `custom` ignores the shape heuristic entirely, for an environment whose
    /// names it reads wrong in either direction.
    #[test]
    fn custom_withholds_exactly_what_it_names() {
        let out = withheld(ShellEnvMode::Custom, &[], &["home", "MY_UNUSUAL_NAME"]);
        assert_eq!(out, ["HOME"], "case-insensitive, and nothing inferred");
        assert!(
            !out.iter().any(|n| n == "ANTHROPIC_API_KEY"),
            "the heuristic is off, so a credential passes unless named"
        );
    }

    #[test]
    fn inherit_withholds_nothing() {
        assert!(withheld(ShellEnvMode::Inherit, &[], &["PATH"]).is_empty());
    }

    /// `allow_env_vars` wins under every mode. One list with one meaning is
    /// worth more than a second list that means almost the same thing.
    #[test]
    fn an_allowlisted_name_is_handed_over_under_every_mode() {
        for mode in [
            ShellEnvMode::Filtered,
            ShellEnvMode::Strict,
            ShellEnvMode::Custom,
        ] {
            let out = withheld(mode, &["anthropic_api_key", "HOME"], &["HOME"]);
            assert!(
                !out.iter().any(|n| n == "ANTHROPIC_API_KEY"),
                "{mode:?} should honour allow_env_vars"
            );
            assert!(!out.iter().any(|n| n == "HOME"), "{mode:?}");
        }
    }

    /// The names a cloned repository must not be able to set. Each one decides
    /// where configuration is read from or what gets executed, which is a
    /// different question from whether it holds a secret.
    #[test]
    fn a_dot_env_may_not_steer_the_process() {
        for name in [
            "LEVIATH_CONFIG_PATH",
            "LEVIATH_HOME",
            "LEVIATH_API_TOKEN",
            "LEVIATH_RUNS_DIR",
            "PATH",
            "SHELL",
            "EDITOR",
            "VISUAL",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
        ] {
            assert!(
                !dotenv_var_allowed(name),
                "{name} must not be settable from a repository's .env"
            );
            // Case is not a way around it.
            assert!(!dotenv_var_allowed(&name.to_ascii_lowercase()));
        }
    }

    /// The credentials this feature exists to load still load. Reusing
    /// `is_sensitive_env_name` here would have refused every one of them and
    /// made `.env` support pointless.
    #[test]
    fn a_dot_env_may_still_carry_credentials_and_ordinary_config() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "DATABASE_URL",
            "AWS_SECRET_ACCESS_KEY",
            "OLLAMA_HOST",
            "MY_APP_REGION",
            "RUST_LOG",
        ] {
            assert!(dotenv_var_allowed(name), "{name} should still load");
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

    /// The list is the whole of what stands between a script tool or an MCP
    /// header and a credential, so a common secret name it misses is a leak.
    /// `KEY` was absent, so `OPENAI_KEY` sailed through while `OPENAI_API_KEY`
    /// was caught.
    #[test]
    fn common_secret_names_are_all_recognised() {
        for name in [
            "OPENAI_KEY",
            "ENCRYPTION_KEY",
            "MASTER_KEY",
            "DEPLOY_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "GITHUB_PAT",
            "SENTRY_DSN",
            "NETRC",
            "KUBECONFIG",
            "npm_password",
        ] {
            assert!(
                is_sensitive_env_name(name),
                "{name} must be treated as a secret"
            );
        }

        // And the list has not become "everything": ordinary variables an agent
        // legitimately reads still pass.
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "TERM",
            "TZ",
            "EDITOR",
            "OLLAMA_HOST",
        ] {
            assert!(!is_sensitive_env_name(name), "{name} is not a secret");
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
        // Not sensitive, but still not a child's business by default - the point
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
