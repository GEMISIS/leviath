//! Which providers this build carries.
//!
//! Each provider is a cargo feature, all on by default. Everything outside
//! this crate asks here rather than reading the features itself, so a
//! provider left out of the build is one answer in one place: the config
//! layer, `lev doctor` and a run that routes to it all say the same thing.

use crate::ProviderError;

/// Every provider kind this crate knows, with the cargo feature that builds
/// it. A kind is what a config's `[providers.<kind>]` table is named.
pub const KNOWN: &[(&str, &str)] = &[
    ("anthropic", "anthropic"),
    ("openai", "openai"),
    ("codex", "openai-subscription"),
    ("xai", "xai"),
    ("grok", "xai-subscription"),
    ("google", "google"),
    ("openrouter", "openrouter"),
    ("bedrock", "bedrock"),
    ("meta", "meta"),
    ("ollama", "ollama"),
    ("meshy", "meshy"),
];

/// The provider kinds this build carries, in [`KNOWN`] order.
pub const COMPILED: &[&str] = &[
    #[cfg(feature = "anthropic")]
    "anthropic",
    #[cfg(feature = "openai")]
    "openai",
    #[cfg(feature = "openai-subscription")]
    "codex",
    #[cfg(feature = "xai")]
    "xai",
    #[cfg(feature = "xai-subscription")]
    "grok",
    #[cfg(feature = "google")]
    "google",
    #[cfg(feature = "openrouter")]
    "openrouter",
    #[cfg(feature = "bedrock")]
    "bedrock",
    #[cfg(feature = "meta")]
    "meta",
    #[cfg(feature = "ollama")]
    "ollama",
    #[cfg(feature = "meshy")]
    "meshy",
];

/// Whether this build can run providers written as Rhai scripts.
pub const SCRIPTED: bool = cfg!(feature = "rhai");

/// Whether this build carries the provider `kind`. False for a kind this
/// crate does not know at all.
pub fn is_compiled(kind: &str) -> bool {
    COMPILED.contains(&kind)
}

/// The cargo feature that builds `kind`, or `None` for a kind this crate does
/// not know.
pub fn feature_for(kind: &str) -> Option<&'static str> {
    KNOWN
        .iter()
        .find(|(known, _)| *known == kind)
        .map(|(_, feature)| *feature)
}

/// Why `kind` cannot be used here, when the reason is that this build left it
/// out: `None` for a kind that is built in and for one this crate does not
/// know.
pub fn missing(kind: &str) -> Option<String> {
    missing_in(kind, COMPILED)
}

/// [`missing`] against a given set of built-in kinds, so a test can ask about
/// a build it is not.
pub(crate) fn missing_in(kind: &str, compiled: &[&str]) -> Option<String> {
    let feature = feature_for(kind).filter(|_| !compiled.contains(&kind))?;
    Some(format!(
        "provider \"{kind}\" is not built into this lev (rebuild with --features {feature})"
    ))
}

/// What to do about a provider a run needs and nothing registered: rebuild
/// when this build left it out, configure it otherwise.
pub fn remedy(kind: &str) -> String {
    missing(kind).unwrap_or_else(|| "add it to config.toml (or run `lev setup`)".to_string())
}

/// What `lev --version` adds below the version number: the providers this
/// build carries, on a line of its own, when it carries fewer than all of
/// them. Empty for a build with every provider.
pub fn version_note() -> String {
    version_note_in(COMPILED, SCRIPTED)
}

/// [`version_note`] for a build with `compiled` and, when `scripted`, Rhai
/// script providers.
pub(crate) fn version_note_in(compiled: &[&str], scripted: bool) -> String {
    let mut names = compiled.to_vec();
    if scripted {
        names.push("rhai");
    }
    match (compiled.len() == KNOWN.len() && scripted, names.is_empty()) {
        (true, _) => String::new(),
        (false, true) => "\nproviders: none".to_string(),
        (false, false) => format!("\nproviders: {}", names.join(", ")),
    }
}

/// The error for a provider this build left out.
pub fn not_built(kind: &str) -> ProviderError {
    ProviderError::Other(remedy(kind))
}

/// Why script providers cannot be used here, when this build left them out.
pub fn scripts_missing() -> Option<String> {
    scripts_missing_in(SCRIPTED)
}

/// [`scripts_missing`] for a build that does or does not carry them.
pub(crate) fn scripts_missing_in(scripted: bool) -> Option<String> {
    (!scripted).then(|| {
        "script providers are not built into this lev (rebuild with --features rhai)".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_kind_names_a_feature_and_compiled_follows_its_order() {
        for (kind, feature) in KNOWN {
            assert_eq!(feature_for(kind), Some(*feature));
        }
        assert_eq!(feature_for("nonsense"), None);
        let order: Vec<&str> = KNOWN
            .iter()
            .map(|(kind, _)| *kind)
            .filter(|kind| is_compiled(kind))
            .collect();
        assert_eq!(order, COMPILED);
    }

    #[test]
    fn a_kind_left_out_names_the_feature_that_builds_it() {
        assert_eq!(
            missing_in("codex", &["anthropic"]).as_deref(),
            Some(
                "provider \"codex\" is not built into this lev \
                 (rebuild with --features openai-subscription)"
            )
        );
        assert_eq!(missing_in("anthropic", &["anthropic"]), None);
        assert_eq!(missing_in("nonsense", &[]), None);
    }

    #[test]
    fn the_version_names_the_providers_only_when_some_are_left_out() {
        let all: Vec<&str> = KNOWN.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(version_note_in(&all, true), "");
        assert_eq!(
            version_note_in(&all, false),
            format!("\nproviders: {}", all.join(", "))
        );
        assert_eq!(
            version_note_in(&["anthropic", "openai"], true),
            "\nproviders: anthropic, openai, rhai"
        );
        assert_eq!(version_note_in(&[], false), "\nproviders: none");
    }

    #[test]
    fn script_providers_left_out_say_how_to_get_them_back() {
        assert_eq!(scripts_missing_in(true), None);
        assert!(
            scripts_missing_in(false)
                .unwrap()
                .contains("--features rhai")
        );
    }

    #[test]
    #[cfg(feature = "providers")]
    fn the_default_build_carries_everything() {
        // The coverage and test runs build with every feature, so this is
        // the answer they see; a smaller build answers per kind.
        assert_eq!(scripts_missing(), None);
        for (kind, _) in KNOWN {
            assert_eq!(missing(kind), None);
            assert_eq!(remedy(kind), "add it to config.toml (or run `lev setup`)");
        }
        assert!(not_built("anthropic").to_string().contains("lev setup"));
        assert_eq!(version_note(), "");
    }
}
