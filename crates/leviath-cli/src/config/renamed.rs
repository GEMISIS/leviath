//! Config keys that changed name or meaning, and the one table every surface
//! reads them from.
//!
//! A renamed key has to be handled in four places or it is handled in none: the
//! loader (so an install that never runs `lev update` keeps working), the
//! unread-key warning (so the old name is not reported as a typo), `lev
//! doctor` (so the reader who asks is told), and `lev update` (so the file is
//! rewritten with the user watching). Each of those reads [`RENAMED_KEYS`], so
//! the next rename is one entry here and nothing else.

/// One config key that now loads under another name.
#[derive(Debug, PartialEq, Eq)]
pub struct RenamedKey {
    /// The table the key sits in, spelled as its header is, or `None` for a
    /// key at the top of the file. A name is only ever resolved inside its own
    /// table: `persist` under `[sandbox]` is not the same setting as a
    /// `persist` anywhere else, and a rewrite that matched on the name alone
    /// would give one of them the other's name.
    pub section: Option<&'static str>,
    /// The name a config written before the change carries.
    pub old: &'static str,
    /// The name it is read as now.
    pub new: &'static str,
    /// What changed for the user, in one or two sentences: not just the name,
    /// but what the value now does, so the notice reads as a change of
    /// behaviour when it is one.
    pub note: &'static str,
}

/// The renames this build knows about, oldest first.
///
/// Adding one is adding an entry here. A key that changes meaning without
/// changing name does not fit this table: it belongs in `lev update`'s
/// migrations directly, where the raw document can be inspected.
pub const RENAMED_KEYS: &[RenamedKey] = &[
    RenamedKey {
        section: None,
        old: "default_model",
        new: "fallback_model",
        note: "`default_model` pinned that one model on every stage, ahead of the models each \
               blueprint names. It now loads as `fallback_model`: every stage runs the model its \
               blueprint names again, and this model is used only by a stage none of whose own \
               models is configured here. To keep the old behaviour, set `override_model` to it \
               instead.",
    },
    RenamedKey {
        section: Some("sandbox"),
        old: "persist",
        new: "keep_warm",
        note: "It keeps one container warm across a run's stages rather than building one per \
               call. The container is still torn down when the run ends, so `persist` promised a \
               lifetime it never gave. The setting itself is unchanged, and the blueprint key of \
               the same name was renamed with it.",
    },
];

impl RenamedKey {
    /// The old name as a reader finds it in the file: the key alone, or its
    /// table and the key.
    pub fn old_path(&self) -> String {
        path_of(self.section, self.old)
    }

    /// The new name, spelled the same way.
    pub fn new_path(&self) -> String {
        path_of(self.section, self.new)
    }
}

/// A key written the way a notice points at it.
fn path_of(section: Option<&str>, key: &str) -> String {
    match section {
        Some(table) => format!("[{table}] {key}"),
        None => key.to_string(),
    }
}

/// A rename that was applied to a document: the key, and its value as written.
#[derive(Debug, PartialEq, Eq)]
pub struct Renamed {
    /// Which rename.
    pub key: &'static RenamedKey,
    /// The value's text as it appears in the file, quotes and all.
    pub value: String,
}

/// The key a line assigns, and the text of its value. A header, a comment and
/// a blank line assign nothing.
fn assignment(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || key.starts_with('#') || key.starts_with('[') {
        return None;
    }
    Some((key, value.trim()))
}

/// The table a `[header]` line opens, or `None` for any other line.
fn header(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix('[')?
        .strip_suffix(']')
        .map(str::trim)
}

/// Rewrite the text of a config so each old key is spelled with its new name,
/// when the new name is absent, and say which were rewritten.
///
/// Done on the text rather than on a parsed table so that everything else
/// about reading the file is unchanged: a type error still points at its
/// line, and the unread-key diff judges the document exactly as the loader
/// read it. Each line carries the table it sits under, so a same-named key in
/// another table is left alone: it is somebody else's setting.
///
/// The new name wins when both are present: the user wrote the current key on
/// purpose, and the old one is left where the unread-key warning will name it.
pub fn rename_in_text(content: &str) -> (String, Vec<Renamed>) {
    let mut renamed = Vec::new();
    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    let mut open: Option<String> = None;
    let sections: Vec<Option<String>> = lines
        .iter()
        .map(|line| {
            if let Some(name) = header(line) {
                open = Some(name.to_string());
            }
            open.clone()
        })
        .collect();
    for key in RENAMED_KEYS {
        // Only the lines inside this key's own table, so a same-named key
        // under another header is nobody's business here.
        let here = |section: &Option<String>| section.as_deref() == key.section;
        let has_new = lines.iter().zip(&sections).any(|(line, section)| {
            here(section) && assignment(line).is_some_and(|(k, _)| k == key.new)
        });
        if has_new {
            continue;
        }
        for (line, section) in lines.iter_mut().zip(&sections) {
            let Some((k, value)) = assignment(line).filter(|_| here(section)) else {
                continue;
            };
            if k != key.old {
                continue;
            }
            let value = value.to_string();
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            *line = format!("{indent}{} = {value}", key.new);
            renamed.push(Renamed { key, value });
        }
    }
    let mut text = lines.join("\n");
    if content.ends_with('\n') {
        text.push('\n');
    }
    (text, renamed)
}

/// The old keys present in a parsed document, whether or not the new name is
/// there too, with each value as TOML text. What `lev doctor` and `lev update`
/// ask.
pub fn legacy_keys_present(table: &toml::Table) -> Vec<Renamed> {
    RENAMED_KEYS
        .iter()
        .filter_map(|key| {
            let scope = match key.section {
                None => Some(table),
                Some(name) => table.get(name).and_then(toml::Value::as_table),
            }?;
            scope.get(key.old).map(|value| Renamed {
                key,
                value: value.to_string(),
            })
        })
        .collect()
}

/// The notice for one rename: what was read, as what, and how to make the
/// file say so itself.
pub fn notice(renamed: &Renamed) -> String {
    format!(
        "config.toml `{old} = {value}` was read as `{new} = {value}`. {note} Run `lev update` \
         to rewrite the file, or rename the key yourself.",
        old = renamed.key.old_path(),
        new = renamed.key.new_path(),
        value = renamed.value,
        note = renamed.key.note,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_key_is_respelled_in_place_and_reported() {
        let (text, renamed) =
            rename_in_text("default_provider = \"p\"\ndefault_model = \"m\" # pinned\n");
        assert_eq!(
            text,
            "default_provider = \"p\"\nfallback_model = \"m\" # pinned\n"
        );
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].key.old, "default_model");
        assert_eq!(renamed[0].value, "\"m\" # pinned");
    }

    /// The user wrote the current key on purpose, so the old one is left for
    /// the unread-key warning rather than silently discarded or promoted.
    #[test]
    fn the_new_key_wins_when_both_are_present() {
        let text = "default_model = \"old\"\nfallback_model = \"new\"\n";
        let (out, renamed) = rename_in_text(text);
        assert_eq!(out, text);
        assert!(renamed.is_empty());
    }

    /// A same-named key inside a table is somebody else's setting, and a
    /// comment or a header is not an assignment.
    #[test]
    fn only_top_level_assignments_are_renamed() {
        let text = "# default_model = \"c\"\n[model_providers.x]\ndefault_model = \"m\"\n";
        let (out, renamed) = rename_in_text(text);
        assert_eq!(out, text);
        assert!(renamed.is_empty());
        let (out, renamed) = rename_in_text("  default_model=\"m\"\n[limits]\n");
        assert_eq!(out, "  fallback_model = \"m\"\n[limits]\n");
        assert_eq!(renamed.len(), 1);
    }

    /// A key inside a table is renamed inside that table and nowhere else.
    /// `persist` is a real key under `[sandbox]` and a stranger anywhere
    /// above it, so matching on the name alone would rewrite both.
    #[test]
    fn a_key_is_renamed_within_its_own_table() {
        let (out, renamed) =
            rename_in_text("persist = true\n[cache]\npersist = 1\n[sandbox]\npersist = false\n");
        assert_eq!(
            out,
            "persist = true\n[cache]\npersist = 1\n[sandbox]\nkeep_warm = false\n"
        );
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].key.new, "keep_warm");
        assert_eq!(renamed[0].value, "false");
    }

    /// A table the file never opens has nothing to rename, and a line that
    /// only looks like a header does not open one.
    #[test]
    fn a_table_that_is_not_there_renames_nothing() {
        let text = "[sandbox.limits]\npersist = false\n";
        let (out, renamed) = rename_in_text(text);
        assert_eq!(out, text);
        assert!(renamed.is_empty());
        assert_eq!(header("[sandbox]"), Some("sandbox"));
        assert_eq!(header("[sandbox"), None);
        assert_eq!(header("persist = false"), None);
    }

    #[test]
    fn a_document_without_a_trailing_newline_stays_without_one() {
        let (out, renamed) = rename_in_text("default_model = \"m\"");
        assert_eq!(out, "fallback_model = \"m\"");
        assert_eq!(renamed.len(), 1);
        let (out, renamed) = rename_in_text("fallback_model = \"m\"");
        assert_eq!(out, "fallback_model = \"m\"");
        assert!(renamed.is_empty());
    }

    #[test]
    fn legacy_keys_present_reports_the_old_name_even_beside_the_new() {
        let t: toml::Table =
            toml::from_str("default_model = \"old\"\nfallback_model = \"new\"\n").unwrap();
        let present = legacy_keys_present(&t);
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].key.new, "fallback_model");
        assert_eq!(present[0].value, "\"old\"");
        let t: toml::Table = toml::from_str("fallback_model = \"m\"\n").unwrap();
        assert!(legacy_keys_present(&t).is_empty());
    }

    /// A key in a table is looked for in that table: absent when the table is
    /// absent, absent when the name sits at the top of the file instead, and
    /// absent when the table is not a table at all.
    #[test]
    fn legacy_keys_present_looks_inside_the_right_table() {
        let t: toml::Table = toml::from_str("[sandbox]\npersist = false\n").unwrap();
        let present = legacy_keys_present(&t);
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].key.new_path(), "[sandbox] keep_warm");
        assert_eq!(present[0].value, "false");

        let t: toml::Table = toml::from_str("persist = false\n[sandbox]\nmode = \"none\"\n")
            .expect("a stray top-level key is still TOML");
        assert!(legacy_keys_present(&t).is_empty());

        let t: toml::Table = toml::from_str("sandbox = \"none\"\n").unwrap();
        assert!(legacy_keys_present(&t).is_empty());
    }

    /// The notice names both keys, quotes the value as written, carries the
    /// behaviour note, and says how to make the file say so itself.
    #[test]
    fn the_notice_says_what_was_read_as_what_and_how_to_fix_the_file() {
        let text = notice(&Renamed {
            key: &RENAMED_KEYS[0],
            value: "\"qwen\"".to_string(),
        });
        assert!(text.contains("`default_model = \"qwen\"`"), "{text}");
        assert!(text.contains("`fallback_model = \"qwen\"`"), "{text}");
        assert!(text.contains("override_model"), "{text}");
        assert!(text.contains("lev update"), "{text}");
    }
}
