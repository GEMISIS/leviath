//! The user's locale, as a BCP-47 language tag.

/// The host's current locale tag, or `None` when the platform has no answer.
///
/// There is no `std` API for this and the shapes differ per platform: Unix
/// reports through `LC_ALL`/`LC_MESSAGES`/`LANG` (often as `en_US.UTF-8`),
/// macOS answers through CoreFoundation and Windows through
/// `GetUserDefaultLocaleName`. `sys-locale` covers all three, so the branching
/// lives in that crate rather than in a `#[cfg]` here.
///
/// The tag comes back exactly as the platform gave it. Normalizing it is the
/// caller's business, and the caller (`leviath-tools`' `locale_info`) has a pure
/// splitter that is testable against every shape without an OS to ask.
pub fn current_tag() -> Option<String> {
    usable(sys_locale::get_locale())
}

/// A tag the caller can act on, or `None`.
///
/// Blank is not an answer: a locale reported as `""` flows downstream as one
/// nobody can act on, which is worse than saying the platform did not answer.
/// Split out from [`current_tag`] so the rule is testable without a host that
/// happens to be configured the right way - CI runners differ, and a test that
/// only checks the rule when the machine sets a locale checks it nowhere.
fn usable(tag: Option<String>) -> Option<String> {
    tag.filter(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_tag_is_no_answer_at_all() {
        assert_eq!(usable(Some("en-US".to_string())), Some("en-US".to_string()));
        assert_eq!(usable(Some(String::new())), None);
        assert_eq!(usable(Some("   ".to_string())), None);
        assert_eq!(usable(None), None);
    }

    /// The real lookup runs, whatever this host answers. CI runners vary - some
    /// set no locale at all - so there is nothing to assert about the value
    /// beyond the rule above, which is tested against every shape directly.
    #[test]
    fn the_real_lookup_runs_on_this_host() {
        let _ = current_tag();
    }
}
