//! Blueprint keys that changed name, and the one table every surface reads.
//!
//! A renamed key has to be handled in four places or it is handled in none: the
//! parser (so a blueprint written before the change keeps working), the
//! unknown-key check (so the old name is not reported as a typo), the lint (so
//! an author who runs `lev validate` is told), and `lev update` (so the file is
//! rewritten with the user watching). Each of those reads [`RENAMED_KEYS`], so
//! the next rename is one entry here and nothing else.
//!
//! This is the blueprint twin of the config table in
//! `leviath-cli/src/config/renamed.rs`. It is separate because the two documents
//! are separate: a blueprint key lives in a named section, so a rewrite has to
//! know which table it is in rather than matching a top-level line.
//!
//! A key whose *meaning* changed does not belong here. This table promises the
//! value means exactly what it meant before, so a rewrite can be silent.

/// The tables a renamed key can sit in.
///
/// A blueprint nests: `[sandbox]` is a run-wide table and also a per-stage one,
/// and regions are declared at both levels too. So a site is a set of table
/// *shapes* rather than one path, and a key is only ever a rename inside one of
/// them - `persist` in a table nobody here names belongs to someone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// A `[sandbox]` table, run-wide or on a stage.
    Sandbox,
    /// A `[stages.<stage>.tool_routing]` table.
    ToolRouting,
    /// One region's own table under `[context.regions]`, at either level.
    Region,
}

impl Site {
    /// The table paths this site covers, `*` standing for any one name.
    fn shapes(self) -> &'static [&'static [&'static str]] {
        match self {
            Site::Sandbox => &[&["sandbox"], &["stages", "*", "sandbox"]],
            Site::ToolRouting => &[&["stages", "*", "tool_routing"]],
            Site::Region => &[
                &["context", "regions", "*"],
                &["stages", "*", "context", "regions", "*"],
            ],
        }
    }

    /// Whether the table at `path` is one of this site's.
    pub fn holds(self, path: &[&str]) -> bool {
        self.shapes().iter().any(|shape| {
            shape.len() == path.len()
                && shape
                    .iter()
                    .zip(path)
                    .all(|(want, got)| *want == "*" || want == got)
        })
    }
}

/// One blueprint key that now reads under another name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenamedKey {
    /// Where an author finds it, written the way the docs write it.
    pub section: &'static str,
    /// The tables it is read in.
    pub site: Site,
    /// The name a blueprint written before the change carries.
    pub old: &'static str,
    /// The name it is read as now.
    pub new: &'static str,
    /// Why the new name is truer, in one or two sentences. Shown by the lint
    /// and by `lev update`, so it has to explain the setting, not just the
    /// spelling.
    pub note: &'static str,
}

/// The renames this build knows about, oldest first.
///
/// Every one of these was a name that described the wrong thing. `persist` said
/// nothing about what was persisted or for how long, and it meant two unrelated
/// things in two tables; `persistent` sounded like it outlived the run when it
/// only meant the region is not evicted during one.
pub const RENAMED_KEYS: &[RenamedKey] = &[KEEP_RESULTS, KEEP_WARM, PINNED];

/// `[stages.<stage>.tool_routing] persist` is `keep_results`.
pub const KEEP_RESULTS: RenamedKey = RenamedKey {
    section: "[stages.<stage>.tool_routing]",
    site: Site::ToolRouting,
    old: "persist",
    new: "keep_results",
    note: "It decides whether a tool's result stays in the region it was routed to, or goes to \
           `scratch` instead. It never had anything to do with surviving a stage change, which is \
           what `persist` reads as.",
};

/// `[sandbox] persist` is `keep_warm`.
pub const KEEP_WARM: RenamedKey = RenamedKey {
    section: "[sandbox]",
    site: Site::Sandbox,
    old: "persist",
    new: "keep_warm",
    note: "It keeps one container warm across the run's stages rather than building one per call. \
           The container is still torn down when the run ends, so `persist` promised a lifetime it \
           never gave.",
};

/// A custom region's `persistent` is `pinned`.
pub const PINNED: RenamedKey = RenamedKey {
    section: "a region with `kind = \"custom\"`",
    site: Site::Region,
    old: "persistent",
    new: "pinned",
    note: "It makes a custom region behave like a pinned one: never evicted, immune to a `clear` \
           transform, counted as fixed budget. It says nothing about later runs, which is what \
           `persistent` suggests.",
};

/// The names in `allowed` worth offering an author, old spellings dropped.
///
/// A key list a parser matches against holds both spellings, because both are
/// read. The refusal an author sees must not: a list naming `persist` beside
/// `keep_warm` reads as two settings, and the one to reach for is the current
/// one. A name is only dropped when its replacement is in the same list, so a
/// list that has not been through a rename is returned as it is.
pub fn current_names<'a>(allowed: &[&'a str]) -> Vec<&'a str> {
    allowed
        .iter()
        .copied()
        .filter(|name| {
            !RENAMED_KEYS
                .iter()
                .any(|key| key.old == *name && allowed.contains(&key.new))
        })
        .collect()
}

/// One old key found in a blueprint: which rename, the table it is in as that
/// document spells it, and its value as TOML text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Which rename this is.
    pub key: &'static RenamedKey,
    /// The table's dotted path in this document, so the author is pointed at
    /// the line they wrote rather than at the shape in the docs.
    pub path: String,
    /// The value as it was written.
    pub value: String,
    /// The new name is in the same table, so this line is already dead: the
    /// parser reads the new one and this one does nothing.
    pub superseded: bool,
}

/// Every old key a blueprint document still carries, in document order.
///
/// What `lev validate` reports and `lev update` rewrites. Walks the parsed
/// document rather than the text so a key is judged by the table it is really
/// in: a `persist` under `[agent]` is somebody else's and is left alone, and an
/// inline `{ kind = "custom", persistent = true }` is found as readily as a
/// table header is.
pub fn legacy_keys_in(document: &toml::value::Table) -> Vec<Found> {
    let mut found = Vec::new();
    walk(document, &mut Vec::new(), &mut found);
    found
}

/// Visit `table` and every table under it, reporting the old keys each holds.
fn walk<'a>(table: &'a toml::value::Table, path: &mut Vec<&'a str>, found: &mut Vec<Found>) {
    for key in RENAMED_KEYS {
        if !key.site.holds(path) {
            continue;
        }
        let Some(value) = table.get(key.old) else {
            continue;
        };
        found.push(Found {
            key,
            path: path.join("."),
            value: value.to_string(),
            superseded: table.contains_key(key.new),
        });
    }
    for (name, value) in table {
        let Some(child) = value.as_table() else {
            continue;
        };
        path.push(name);
        walk(child, path, found);
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KEEP_RESULTS, KEEP_WARM, PINNED, RENAMED_KEYS, Site, current_names, legacy_keys_in,
    };

    /// The document a blueprint's renamed keys can appear in, every one of
    /// them, at both the run-wide level and on a stage.
    const EVERYWHERE: &str = r#"
[agent]
name = "a"
persist = "not a setting of ours"

[sandbox]
persist = true

[context.regions]
brain = { kind = "custom", script = "c.rhai", persistent = true }

[stages.plan]
model = "m"

[stages.plan.sandbox]
persist = false

[stages.plan.tool_routing]
persist = false

[stages.plan.context.regions]
notes = { kind = "custom", script = "n.rhai", persistent = false }
"#;

    fn parse(text: &str) -> toml::value::Table {
        toml::from_str(text).expect("the fixture is TOML")
    }

    /// No key is renamed twice, and no new name is also an old one.
    ///
    /// A chain would make the rewrite order-dependent: applying the table twice
    /// would move a value a second time, and an author would see a key they
    /// never wrote turn into a third one.
    #[test]
    fn the_table_holds_no_chains() {
        for key in RENAMED_KEYS {
            assert!(
                !RENAMED_KEYS
                    .iter()
                    .any(|other| other.section == key.section && other.old == key.new),
                "{} is both a new name and an old one in {}",
                key.new,
                key.section
            );
            assert_ne!(key.old, key.new, "{} renames to itself", key.old);
            assert!(!key.note.is_empty(), "{} has no note", key.old);
        }
    }

    /// Every site is found at both levels, each under its own new name, and a
    /// same-named key in a table nothing renamed is left where it is.
    ///
    /// `persist` means one thing under `[sandbox]` and another under
    /// `tool_routing`, so a walk that matched on the name alone would give one
    /// of them the other's name - and `[agent] persist` is not ours at all.
    #[test]
    fn every_site_is_found_at_both_levels_and_nothing_else_is() {
        let found = legacy_keys_in(&parse(EVERYWHERE));
        let seen: Vec<(&str, &str, &str)> = found
            .iter()
            .map(|f| (f.path.as_str(), f.key.old, f.key.new))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("sandbox", "persist", "keep_warm"),
                ("context.regions.brain", "persistent", "pinned"),
                ("stages.plan.sandbox", "persist", "keep_warm"),
                ("stages.plan.tool_routing", "persist", "keep_results"),
                ("stages.plan.context.regions.notes", "persistent", "pinned"),
            ]
        );
        assert_eq!(found[0].value, "true");
        assert!(!found[0].superseded);
    }

    /// A key written under both names is reported as already dead: the parser
    /// reads the new one, so the old line does nothing and wants removing
    /// rather than renaming.
    #[test]
    fn a_key_written_twice_is_reported_as_superseded() {
        let found = legacy_keys_in(&parse("[sandbox]\npersist = true\nkeep_warm = false\n"));
        assert_eq!(found.len(), 1);
        assert!(found[0].superseded);
    }

    /// A blueprint with none of them reports none, and a table the shapes do
    /// not name is not walked into by accident.
    #[test]
    fn a_blueprint_that_does_not_use_them_reports_nothing() {
        assert!(legacy_keys_in(&parse("[sandbox]\nkeep_warm = true\n")).is_empty());
        assert!(legacy_keys_in(&parse("[context]\nregions = \"x\"\n")).is_empty());
        assert!(legacy_keys_in(&parse("[regions.brain]\npersistent = true\n")).is_empty());
    }

    /// A site matches on the shape of the whole path, not on its last name.
    #[test]
    fn a_site_matches_the_shape_of_the_path() {
        assert!(Site::Sandbox.holds(&["sandbox"]));
        assert!(Site::Sandbox.holds(&["stages", "plan", "sandbox"]));
        assert!(!Site::Sandbox.holds(&["agent", "sandbox"]));
        assert!(!Site::Sandbox.holds(&["stages", "plan", "sandbox", "limits"]));
        assert!(Site::ToolRouting.holds(&["stages", "p", "tool_routing"]));
        assert!(!Site::ToolRouting.holds(&["tool_routing"]));
        assert!(Site::Region.holds(&["context", "regions", "brain"]));
        assert!(!Site::Region.holds(&["context", "regions"]));
    }

    /// A refusal offers the current names only, and a list with no rename in
    /// it comes back whole.
    #[test]
    fn an_old_spelling_is_accepted_but_never_offered() {
        assert_eq!(
            current_names(&["default_region", "keep_results", "persist"]),
            vec!["default_region", "keep_results"]
        );
        assert_eq!(current_names(&["kind", "image"]), vec!["kind", "image"]);
        // Without its replacement beside it, a name is nobody's old spelling:
        // this list is some other table's, and `persist` is its own key.
        assert_eq!(current_names(&["persist"]), vec!["persist"]);
    }

    /// The parser reads each rename under the entry it is declared with, so
    /// the three surfaces cannot drift apart.
    #[test]
    fn each_entry_is_the_one_its_site_names() {
        assert_eq!(KEEP_RESULTS.site, Site::ToolRouting);
        assert_eq!(KEEP_WARM.site, Site::Sandbox);
        assert_eq!(PINNED.site, Site::Region);
    }
}
