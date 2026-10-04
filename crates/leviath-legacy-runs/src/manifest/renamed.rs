//! Manifest keys that changed name.
//!
//! The parser reads each renamed key under either spelling, so a manifest
//! written before the rename keeps working, and a refusal that lists the keys
//! a table takes offers only the current spelling. [`RENAMED_KEYS`] is the
//! one list both read.
//!
//! A key whose *meaning* changed does not belong here: this list promises the
//! value means exactly what it meant before.

/// One blueprint key that now reads under another name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenamedKey {
    /// Where an author finds it, written the way the docs write it.
    pub section: &'static str,
    /// The name a blueprint written before the change carries.
    pub old: &'static str,
    /// The name it is read as now.
    pub new: &'static str,
    /// Why the new name is truer, in one or two sentences.
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
    old: "persist",
    new: "keep_results",
    note: "It decides whether a tool's result stays in the region it was routed to, or goes to \
           `scratch` instead. It never had anything to do with surviving a stage change, which is \
           what `persist` reads as.",
};

/// `[sandbox] persist` is `keep_warm`.
pub const KEEP_WARM: RenamedKey = RenamedKey {
    section: "[sandbox]",
    old: "persist",
    new: "keep_warm",
    note: "It keeps one container warm across the run's stages rather than building one per call. \
           The container is still torn down when the run ends, so `persist` promised a lifetime it \
           never gave.",
};

/// A custom region's `persistent` is `pinned`.
pub const PINNED: RenamedKey = RenamedKey {
    section: "a region with `kind = \"custom\"`",
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

#[cfg(test)]
mod tests {
    use super::{RENAMED_KEYS, current_names};

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
}
