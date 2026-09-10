//! Editing the rows in `mime_types.toml` from the command line.
//!
//! `lev mime add` and `lev mime remove` change one row at a time in the
//! file beside the config, keeping every other row, comment and blank line
//! as the person wrote them. An edit is checked before it is written by
//! building the registry the file would make (the compiled defaults with
//! the new rows on top, the row's `check` script compiled), so a flag that
//! would leave the file unloadable is refused with the registry's own
//! words and the file is left as it was.

use std::path::Path;

use leviath_core::mime::MimeRegistry;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// What `lev mime add` sets on a row. Every field optional: a row names
/// only what it changes, and an `add` on a row that is already there
/// changes only the fields given.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RowEdit {
    /// `family`.
    pub family: Option<String>,
    /// `text`.
    pub text: Option<bool>,
    /// `tokens`, as its one-key inline table.
    pub tokens: Option<TokenSpec>,
    /// `extensions`.
    pub extensions: Option<Vec<String>>,
    /// `magic`.
    pub magic: Option<String>,
    /// `stand_in`.
    pub stand_in: Option<String>,
    /// `check`. An empty string lifts a broader row's check.
    pub check: Option<String>,
}

/// A token rule as `--tokens` spells it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenSpec {
    /// `per_byte=0.25`.
    PerByte(f64),
    /// `per_pixel=750` or `per_pixel=750,max=1600`.
    PerPixel {
        /// Pixels per token.
        divisor: i64,
        /// The cap, when given.
        max: Option<i64>,
    },
    /// `per_second=32`.
    PerSecond(i64),
    /// `fixed=1000`.
    Fixed(i64),
}

impl TokenSpec {
    /// Read `--tokens per_pixel=750,max=1600`: one rule, with `max` allowed
    /// only beside `per_pixel`.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let mut rule: Option<(String, String)> = None;
        let mut max: Option<i64> = None;
        for pair in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(format!("'{pair}' is not key=value"));
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "max" => {
                    max = Some(
                        value
                            .parse()
                            .map_err(|_| format!("max must be a whole number, got '{value}'"))?,
                    );
                }
                "per_byte" | "per_pixel" | "per_second" | "fixed" => {
                    if rule.is_some() {
                        return Err("name exactly one of per_byte, per_pixel, per_second, fixed"
                            .to_string());
                    }
                    rule = Some((key.to_string(), value.to_string()));
                }
                other => {
                    return Err(format!(
                        "'{other}' is not a token rule: per_byte, per_pixel (with max), \
                         per_second or fixed"
                    ));
                }
            }
        }
        let Some((key, value)) = rule else {
            return Err("name one of per_byte, per_pixel, per_second, fixed".to_string());
        };
        let whole = |value: &str| {
            value
                .parse::<i64>()
                .map_err(|_| format!("{key} must be a whole number, got '{value}'"))
        };
        let spec = match key.as_str() {
            "per_byte" => TokenSpec::PerByte(
                value
                    .parse()
                    .map_err(|_| format!("per_byte must be a number, got '{value}'"))?,
            ),
            "per_pixel" => TokenSpec::PerPixel {
                divisor: whole(&value)?,
                max,
            },
            "per_second" => TokenSpec::PerSecond(whole(&value)?),
            _ => TokenSpec::Fixed(whole(&value)?),
        };
        if max.is_some() && !matches!(spec, TokenSpec::PerPixel { .. }) {
            return Err("max only goes with per_pixel".to_string());
        }
        Ok(spec)
    }

    /// The inline table the row carries.
    fn to_value(&self) -> Value {
        let mut table = InlineTable::new();
        match self {
            TokenSpec::PerByte(rate) => {
                table.insert("per_byte", Value::from(*rate));
            }
            TokenSpec::PerPixel { divisor, max } => {
                table.insert("per_pixel", Value::from(*divisor));
                if let Some(max) = max {
                    table.insert("max", Value::from(*max));
                }
            }
            TokenSpec::PerSecond(rate) => {
                table.insert("per_second", Value::from(*rate));
            }
            TokenSpec::Fixed(n) => {
                table.insert("fixed", Value::from(*n));
            }
        }
        Value::InlineTable(table)
    }
}

/// What an `add` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Added {
    /// The row was not there before.
    Created,
    /// The row was there; the fields given were set on it.
    Updated,
}

/// The document `path` holds, or an empty one when there is no file.
fn read_doc(path: &Path) -> Result<DocumentMut, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // No file, or a path under something that is not a directory: the
        // write below says what is in the way.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            String::new()
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    text.parse::<DocumentMut>()
        .map_err(|e| format!("{} is not TOML: {}", path.display(), e.message()))
}

/// The table the rows live in: a `[mime_types]` wrapper when the file
/// carries one (a block moved out of the config), else the document itself.
fn rows_mut(doc: &mut DocumentMut) -> &mut dyn TableLike {
    match doc
        .get("mime_types")
        .and_then(Item::as_table_like)
        .is_some()
    {
        true => doc["mime_types"]
            .as_table_like_mut()
            .expect("checked just above"),
        false => doc.as_table_mut(),
    }
}

/// The rows a document makes, the way the registry reads them.
fn rows_of(doc: &DocumentMut) -> toml::Table {
    // A document `toml_edit` holds always reads back as TOML.
    let mut table: toml::Table =
        toml::from_str(&doc.to_string()).expect("an edited document is still TOML");
    if let Some(toml::Value::Table(wrapped)) = table.remove("mime_types") {
        table.extend(wrapped);
    }
    table
}

/// Check that the file `doc` would make loads: every row layers over the
/// compiled defaults, and every check it names compiles from beside the file.
fn check_loads(doc: &DocumentMut, path: &Path) -> Result<(), String> {
    let rows = rows_of(doc);
    let mut registry = MimeRegistry::builtin();
    registry
        .layer(&rows, crate::config::MIME_TYPES_FILE)
        .map_err(|e| e.to_string())?;
    let dir = path.parent().unwrap_or(Path::new("."));
    crate::config::attach_checks(&mut registry, dir).map_err(|e| e.to_string())
}

/// Write `doc` to `path`, creating the directory.
fn write_doc(doc: &DocumentMut, path: &Path) -> Result<(), String> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    std::fs::write(path, doc.to_string())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Put `edit` on the row for `key` in `path`, creating the row (and the
/// file) when there is none, and say which it was.
pub(crate) fn add_row(path: &Path, key: &str, edit: &RowEdit) -> Result<Added, String> {
    let mut doc = read_doc(path)?;
    let rows = rows_mut(&mut doc);
    let added = match rows.get(key).and_then(Item::as_table_like).is_some() {
        true => Added::Updated,
        false => {
            let mut table = Table::new();
            table.set_implicit(false);
            rows.insert(key, Item::Table(table));
            Added::Created
        }
    };
    let row = rows
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .expect("a table was found or inserted just above");
    if let Some(family) = &edit.family {
        row.insert("family", Item::Value(Value::from(family.as_str())));
    }
    if let Some(text) = edit.text {
        row.insert("text", Item::Value(Value::from(text)));
    }
    if let Some(tokens) = &edit.tokens {
        row.insert("tokens", Item::Value(tokens.to_value()));
    }
    if let Some(extensions) = &edit.extensions {
        let mut array = Array::new();
        for ext in extensions {
            array.push(ext.as_str());
        }
        row.insert("extensions", Item::Value(Value::Array(array)));
    }
    if let Some(magic) = &edit.magic {
        row.insert("magic", Item::Value(Value::from(magic.as_str())));
    }
    if let Some(stand_in) = &edit.stand_in {
        row.insert("stand_in", Item::Value(Value::from(stand_in.as_str())));
    }
    if let Some(check) = &edit.check {
        row.insert("check", Item::Value(Value::from(check.as_str())));
    }
    check_loads(&doc, path)?;
    write_doc(&doc, path)?;
    Ok(added)
}

/// Take the row for `key` out of `path`.
pub(crate) fn remove_row(path: &Path, key: &str) -> Result<(), String> {
    let mut doc = read_doc(path)?;
    let rows = rows_mut(&mut doc);
    if rows.remove(key).is_none() {
        return Err(format!(
            "no row for {key} in {}; `lev mime list` shows every row and where it lives",
            path.display()
        ));
    }
    write_doc(&doc, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_specs_parse_and_refuse() {
        assert_eq!(
            TokenSpec::parse("per_byte=0.25"),
            Ok(TokenSpec::PerByte(0.25))
        );
        assert_eq!(
            TokenSpec::parse(" per_pixel=750 , max=1600 "),
            Ok(TokenSpec::PerPixel {
                divisor: 750,
                max: Some(1600)
            })
        );
        assert_eq!(
            TokenSpec::parse("per_pixel=750"),
            Ok(TokenSpec::PerPixel {
                divisor: 750,
                max: None
            })
        );
        assert_eq!(
            TokenSpec::parse("per_second=32"),
            Ok(TokenSpec::PerSecond(32))
        );
        assert_eq!(TokenSpec::parse("fixed=1000"), Ok(TokenSpec::Fixed(1000)));
        for (bad, needle) in [
            ("", "name one of"),
            ("max=3", "name one of"),
            ("per_byte", "not key=value"),
            ("per_byte=x", "per_byte must be a number"),
            ("per_pixel=x", "per_pixel must be a whole number"),
            ("per_pixel=1,max=x", "max must be a whole number"),
            ("per_second=x", "per_second must be a whole number"),
            ("fixed=x", "fixed must be a whole number"),
            ("fixed=1,per_second=2", "exactly one of"),
            ("fixed=1,max=2", "max only goes with per_pixel"),
            ("rate=1", "not a token rule"),
        ] {
            let err = TokenSpec::parse(bad).unwrap_err();
            assert!(err.contains(needle), "{bad}: {err}");
        }
        // Each rule writes its own key.
        for (spec, key) in [
            (TokenSpec::PerByte(0.5), "per_byte"),
            (TokenSpec::PerSecond(1), "per_second"),
            (TokenSpec::Fixed(2), "fixed"),
            (
                TokenSpec::PerPixel {
                    divisor: 3,
                    max: None,
                },
                "per_pixel",
            ),
        ] {
            let table = spec.to_value();
            let inline = table.as_inline_table().unwrap();
            assert!(inline.contains_key(key));
            assert!(!inline.contains_key("max"));
        }
    }

    /// A new file, a row added, a second `add` that only touches what it
    /// names, and a remove that leaves the neighbours and comments alone.
    #[test]
    fn rows_are_added_updated_and_removed_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("mime_types.toml");
        let edit = RowEdit {
            family: Some("model".to_string()),
            extensions: Some(vec!["scene".to_string()]),
            magic: Some("41434D45".to_string()),
            tokens: Some(TokenSpec::PerPixel {
                divisor: 750,
                max: Some(1600),
            }),
            ..RowEdit::default()
        };
        assert_eq!(
            add_row(&path, "application/x-acme-scene", &edit).unwrap(),
            Added::Created
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[\"application/x-acme-scene\"]"), "{text}");
        assert!(text.contains("family = \"model\""), "{text}");
        assert!(
            text.contains("tokens = { per_pixel = 750, max = 1600 }"),
            "{text}"
        );

        // A comment and a neighbour survive an update that sets two fields.
        std::fs::write(
            &path,
            format!("# mine\n[\"model/obj\"]\ntext = true\n\n{text}"),
        )
        .unwrap();
        let update = RowEdit {
            text: Some(false),
            stand_in: Some("[{type}] {name}".to_string()),
            check: Some(String::new()),
            ..RowEdit::default()
        };
        assert_eq!(
            add_row(&path, "application/x-acme-scene", &update).unwrap(),
            Added::Updated
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# mine\n[\"model/obj\"]\ntext = true\n"),
            "{text}"
        );
        assert!(text.contains("family = \"model\""), "kept: {text}");
        assert!(text.contains("text = false"), "{text}");
        assert!(text.contains("check = \"\""), "{text}");
        assert!(text.contains("stand_in = \"[{type}] {name}\""), "{text}");

        remove_row(&path, "application/x-acme-scene").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("acme"), "{text}");
        assert!(text.contains("[\"model/obj\"]"), "{text}");
        let err = remove_row(&path, "application/x-acme-scene").unwrap_err();
        assert!(err.contains("no row for application/x-acme-scene"), "{err}");
    }

    /// A file carrying a `[mime_types]` wrapper is edited inside it, so the
    /// block keeps loading the way the config reads it.
    #[test]
    fn a_wrapped_file_is_edited_inside_the_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mime_types.toml");
        std::fs::write(&path, "[mime_types.\"x/y\"]\nfamily = \"a\"\n").unwrap();
        let edit = RowEdit {
            family: Some("b".to_string()),
            ..RowEdit::default()
        };
        assert_eq!(add_row(&path, "x/z", &edit).unwrap(), Added::Created);
        let rows = crate::config::rows_in_file(&path).unwrap().unwrap();
        assert!(
            rows.contains_key("x/y") && rows.contains_key("x/z"),
            "{rows:?}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("[mime_types.\"x/z\"]"),
            "written under the wrapper"
        );
        remove_row(&path, "x/y").unwrap();
        let rows = crate::config::rows_in_file(&path).unwrap().unwrap();
        assert_eq!(rows.keys().collect::<Vec<_>>(), vec!["x/z"]);
    }

    /// An edit that would leave the file unloadable is refused with the
    /// registry's words, and the file is left as it was.
    #[test]
    fn an_edit_that_would_not_load_is_refused_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mime_types.toml");
        std::fs::write(&path, "[\"model/obj\"]\ntext = true\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let bad_key = add_row(&path, "png", &RowEdit::default()).unwrap_err();
        assert!(bad_key.contains("mime_types key png"), "{bad_key}");
        let bad_magic = add_row(
            &path,
            "image/png",
            &RowEdit {
                magic: Some("zz".to_string()),
                ..RowEdit::default()
            },
        )
        .unwrap_err();
        assert!(bad_magic.contains("magic must be hex"), "{bad_magic}");
        let no_script = add_row(
            &path,
            "image/png",
            &RowEdit {
                check: Some("checks/gone.rhai".to_string()),
                ..RowEdit::default()
            },
        )
        .unwrap_err();
        assert!(no_script.contains("cannot read"), "{no_script}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // A check that is there is compiled and accepted.
        std::fs::create_dir_all(dir.path().join("checks")).unwrap();
        std::fs::write(
            dir.path().join("checks/png.rhai"),
            "fn check(bytes, mime_type) { () }",
        )
        .unwrap();
        add_row(
            &path,
            "image/png",
            &RowEdit {
                check: Some("checks/png.rhai".to_string()),
                ..RowEdit::default()
            },
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("check = \"checks/png.rhai\"")
        );

        // A file that is not TOML, or cannot be read, is named.
        std::fs::write(&path, "= = =\n").unwrap();
        let err = add_row(&path, "x/y", &RowEdit::default()).unwrap_err();
        assert!(err.contains("is not TOML"), "{err}");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = remove_row(&path, "x/y").unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
        // A key holding something that is not a table becomes a row.
        let flat = dir.path().join("flat.toml");
        std::fs::write(&flat, "\"x/y\" = 3\n").unwrap();
        assert_eq!(
            add_row(&flat, "x/y", &RowEdit::default()).unwrap(),
            Added::Created
        );
        assert!(
            std::fs::read_to_string(&flat)
                .unwrap()
                .contains("[\"x/y\"]")
        );
        // A directory that cannot be created, and a path that cannot be
        // written.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let err = add_row(
            &blocker.join("sub").join("m.toml"),
            "x/y",
            &RowEdit::default(),
        )
        .unwrap_err();
        assert!(err.contains("cannot create"), "{err}");
        let sealed = dir.path().join("sealed.toml");
        std::fs::write(&sealed, "[\"x/y\"]\nfamily = \"a\"\n").unwrap();
        let mut perms = std::fs::metadata(&sealed).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&sealed, perms).unwrap();
        let err = remove_row(&sealed, "x/y").unwrap_err();
        assert!(err.contains("cannot write"), "{err}");
    }
}
