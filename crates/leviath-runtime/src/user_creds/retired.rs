//! `default_provider`, the config key the provider order replaced.
//!
//! The key named one provider for a bare model name; `[providers]
//! provider_order` names all of them, best first, and is the only routing
//! preference there is. A file that still carries the key keeps routing where
//! it did: the name is read as the head of the order. Both readers of the file
//! (this crate's loader and `lev`'s) fold it through here, and `lev update`
//! rewrites the file.

/// The key, at the top of the file.
pub const DEFAULT_PROVIDER: &str = "default_provider";

/// The provider `default_provider` names in a parsed document, when it names
/// one. A blank value, or one that is not a string, names nothing.
pub fn default_provider_in(table: &toml::Table) -> Option<String> {
    table
        .get(DEFAULT_PROVIDER)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// [`default_provider_in`], for a file's text. Text that does not parse names
/// nothing: the typed load that follows reports it.
pub fn default_provider_in_text(text: &str) -> Option<String> {
    default_provider_in(&text.parse::<toml::Table>().ok()?)
}

/// Put `old`, what `default_provider` named, at the head of `order`, and say
/// so. `None` when the file did not carry the key.
///
/// At the head whatever the order already holds, because the head is the
/// provider `override_model` and `fallback_model` are asked of, and those two
/// were asked of `default_provider`.
pub fn fold_default_provider(old: Option<String>, order: &mut Vec<String>) -> Option<String> {
    let old = old?;
    order.retain(|name| *name != old);
    order.insert(0, old.clone());
    Some(format!(
        "config.toml `{DEFAULT_PROVIDER} = \"{old}\"` was read as the first entry of \
         `[providers] provider_order`, which is now [{}]. `{DEFAULT_PROVIDER}` is no longer a \
         setting: the provider order alone decides where a bare model name runs. Run `lev \
         update` to rewrite the file, or move the name yourself.",
        order.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_old_key_names_a_provider_only_as_a_non_blank_string() {
        let named = |text: &str| default_provider_in_text(text);
        assert_eq!(
            named("default_provider = \"openai\"\n"),
            Some("openai".into())
        );
        assert_eq!(named("default_provider = \"  \"\n"), None);
        assert_eq!(named("default_provider = 7\n"), None);
        assert_eq!(named("override_model = \"m\"\n"), None);
        assert_eq!(
            named("default_provider = \n"),
            None,
            "text that does not parse"
        );
    }

    #[test]
    fn a_file_without_the_key_is_left_alone() {
        let mut listed = order(&["openai"]);
        assert_eq!(fold_default_provider(None, &mut listed), None);
        assert_eq!(listed, order(&["openai"]));
    }

    #[test]
    fn the_old_default_becomes_the_whole_order_when_there_was_none() {
        let mut listed = Vec::new();
        let notice = fold_default_provider(Some("openai".into()), &mut listed).unwrap();
        assert_eq!(listed, order(&["openai"]));
        assert!(notice.contains("default_provider = \"openai\""), "{notice}");
        assert!(notice.contains("now [openai]"), "{notice}");
        assert!(notice.contains("lev update"), "{notice}");
    }

    #[test]
    fn the_old_default_leads_an_order_that_did_not_start_with_it() {
        // Listed further down: moved, not repeated.
        let mut listed = order(&["codex", "openai"]);
        let notice = fold_default_provider(Some("openai".into()), &mut listed).unwrap();
        assert_eq!(listed, order(&["openai", "codex"]));
        assert!(notice.contains("now [openai, codex]"), "{notice}");

        // Not listed at all: put first.
        let mut listed = order(&["codex"]);
        fold_default_provider(Some("openai".into()), &mut listed);
        assert_eq!(listed, order(&["openai", "codex"]));

        // Already first: the order is unchanged.
        let mut listed = order(&["openai", "codex"]);
        fold_default_provider(Some("openai".into()), &mut listed);
        assert_eq!(listed, order(&["openai", "codex"]));
    }
}
