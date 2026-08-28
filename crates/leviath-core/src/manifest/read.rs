//! Reading one typed field off a TOML value.
//!
//! The manifest parser asks "the string under this key, if there is one"
//! well over a hundred times, and every ask used to be the same
//! `v.get(key).and_then(|v| v.as_str())` spelled out in place. These are
//! that ask, once per type, so a site reads as what it wants rather than how
//! it gets it. Each returns `None` both for an absent key and for a value of
//! the wrong type, exactly as the inline form did; a caller that wants to
//! tell those apart still looks at the value itself.

/// Something with named fields: a TOML value (which may be a table) or a
/// table itself. The parser holds both, depending on how far into the
/// document it has descended, and asks the same questions of either.
pub(super) trait Fields {
    /// The value under `key`, if any.
    fn field(&self, key: &str) -> Option<&toml::Value>;
}

impl Fields for toml::Value {
    fn field(&self, key: &str) -> Option<&toml::Value> {
        self.get(key)
    }
}

impl Fields for toml::Table {
    fn field(&self, key: &str) -> Option<&toml::Value> {
        self.get(key)
    }
}

/// The string under `key`, if present and a string.
pub(super) fn str_of<'a>(v: &'a impl Fields, key: &str) -> Option<&'a str> {
    v.field(key).and_then(|x| x.as_str())
}

/// A required-shaped string field, defaulting to empty when absent (the value's
/// meaning is validated later by `Blueprint::validate`).
pub(super) fn str_field(v: &impl Fields, key: &str) -> String {
    str_of(v, key).unwrap_or_default().to_string()
}

/// The boolean under `key`, if present and a boolean.
pub(super) fn bool_of(v: &impl Fields, key: &str) -> Option<bool> {
    v.field(key).and_then(|x| x.as_bool())
}

/// The integer under `key`, if present and an integer. Exactly `as_integer`:
/// no range check, since each caller decides what a negative means.
pub(super) fn int_of(v: &impl Fields, key: &str) -> Option<i64> {
    v.field(key).and_then(|x| x.as_integer())
}

/// The array under `key`, if present and an array.
pub(super) fn array_of<'a>(v: &'a impl Fields, key: &str) -> Option<&'a Vec<toml::Value>> {
    v.field(key).and_then(|x| x.as_array())
}

/// The table under `key`, if present and a table.
pub(super) fn table_of<'a>(v: &'a impl Fields, key: &str) -> Option<&'a toml::Table> {
    v.field(key).and_then(|x| x.as_table())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value() -> toml::Value {
        toml::from_str(
            r#"
            s = "text"
            b = true
            i = -3
            a = [1, 2]
            [t]
            inner = 1
            "#,
        )
        .unwrap()
    }

    fn check(v: &impl Fields) {
        assert_eq!(str_of(v, "s"), Some("text"));
        assert_eq!(bool_of(v, "b"), Some(true));
        assert_eq!(int_of(v, "i"), Some(-3));
        assert_eq!(array_of(v, "a").map(Vec::len), Some(2));
        assert!(table_of(v, "t").is_some_and(|t| t.contains_key("inner")));
        // Wrong type reads as absent, as the inline form did.
        assert_eq!(str_of(v, "b"), None);
        assert_eq!(bool_of(v, "s"), None);
        assert_eq!(int_of(v, "s"), None);
        assert!(array_of(v, "t").is_none());
        assert!(table_of(v, "a").is_none());
        assert_eq!(str_field(v, "s"), "text");
        assert_eq!(str_field(v, "missing"), "");
        assert_eq!(str_field(v, "i"), "");
    }

    #[test]
    fn each_helper_reads_its_own_type_off_a_value() {
        check(&value());
    }

    #[test]
    fn each_helper_reads_its_own_type_off_a_table() {
        let table = value().as_table().cloned().expect("a document is a table");
        check(&table);
    }
}
