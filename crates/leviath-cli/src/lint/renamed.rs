//! The lint for blueprint keys that changed name.
//!
//! The parser still reads the old names, so a blueprint carrying one works
//! exactly as it did. This is how its author finds out anyway, rather than
//! discovering it the next time they read the docs and cannot find the key
//! they wrote.

use leviath_core::manifest::renamed::{Found, legacy_keys_in};

use super::{LintFinding, LintSeverity};

/// Report every old key the manifest text still carries.
///
/// Reads the text again rather than the parsed [`Blueprint`], because by then
/// both spellings have become the same field and the question - which one did
/// the author write - is no longer answerable.
///
/// A note, not a warning: nothing is wrong, the run will do exactly what the
/// file says. The exception is a key written under both names, where the old
/// line does nothing at all, which is a warning because the author plainly
/// believes it does something.
///
/// [`Blueprint`]: leviath_core::Blueprint
pub(super) fn lint_renamed_keys(content: &str) -> Vec<LintFinding> {
    let Ok(document) = toml::from_str::<toml::value::Table>(content) else {
        // Not TOML, so whoever failed to parse it has already said so in terms
        // that point at the line. A second, vaguer complaint would bury it.
        return Vec::new();
    };
    legacy_keys_in(&document).iter().map(finding).collect()
}

/// The finding for one old key.
fn finding(found: &Found) -> LintFinding {
    let Found {
        key, path, value, ..
    } = found;
    let where_ = format!("[{path}] {}", key.old);
    if found.superseded {
        return LintFinding::new(
            LintSeverity::Warning,
            "renamed-key-superseded",
            format!(
                "`{where_} = {value}` does nothing: the table also sets `{}`, which is the \
                 name this build reads",
                key.new
            ),
        )
        .with_fix(format!("delete the `{}` line", key.old));
    }
    LintFinding::new(
        LintSeverity::Note,
        "renamed-key",
        format!(
            "`{where_} = {value}` is read as `{new} = {value}`. {note}",
            new = key.new,
            note = key.note,
        ),
    )
    .with_fix(format!(
        "rename it to `{}`, or run `lev update` to rewrite the blueprint",
        key.new
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The note names both spellings, carries the value as written and the
    /// reason the new name is truer, and says how to fix the file.
    #[test]
    fn an_old_key_is_a_note_that_explains_itself() {
        let findings = lint_renamed_keys("[sandbox]\npersist = true\n");
        assert_eq!(findings.len(), 1);
        let it = &findings[0];
        assert_eq!(it.severity, LintSeverity::Note);
        assert_eq!(it.code, "renamed-key");
        assert!(it.message.contains("`[sandbox] persist = true`"), "{it:?}");
        assert!(it.message.contains("`keep_warm = true`"), "{it:?}");
        assert!(it.message.contains("torn down when the run ends"), "{it:?}");
        assert!(
            it.fix.as_deref().is_some_and(|f| f.contains("lev update")),
            "{it:?}"
        );
    }

    /// Both spellings in one table is a warning, because the old line is dead
    /// and the author has no way of knowing that from the file.
    #[test]
    fn both_spellings_at_once_is_a_warning_to_delete_one() {
        let findings =
            lint_renamed_keys("[stages.plan.tool_routing]\npersist = true\nkeep_results = false\n");
        assert_eq!(findings.len(), 1);
        let it = &findings[0];
        assert_eq!(it.severity, LintSeverity::Warning);
        assert_eq!(it.code, "renamed-key-superseded");
        assert!(it.message.contains("does nothing"), "{it:?}");
        assert_eq!(it.fix.as_deref(), Some("delete the `persist` line"));
    }

    /// A blueprint on the current names says nothing, and neither does one
    /// that is not TOML at all.
    #[test]
    fn a_current_blueprint_and_an_unparseable_one_both_report_nothing() {
        assert!(lint_renamed_keys("[sandbox]\nkeep_warm = true\n").is_empty());
        assert!(lint_renamed_keys("this is not [[[ toml").is_empty());
    }
}
