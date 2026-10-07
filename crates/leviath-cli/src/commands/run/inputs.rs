//! Reading an input typed on the command line by the type its blueprint
//! declares.
//!
//! `lev run coder --input depth=3 --input tags=a,b --input photo=@hero.png`
//! hands every value over as text. Each is read here as the wire would carry
//! it for its declared type, so the daemon checks a number as a number and a
//! list as a list:
//!
//! | Declared type | On the command line |
//! |---|---|
//! | `text` | the text, or `@file` for a file's text |
//! | `bool` | `true` or `false` (also `yes`/`no`, `on`/`off`, `1`/`0`) |
//! | `int`, `float` | a number |
//! | `list` | comma-separated items, or a JSON array |
//! | `record` | a JSON object |
//! | `file` | `@path` or a bare path, attached for the input |
//! | anything else | the text, which the daemon checks |
//!
//! A text input given `@file` for a file that is not text attaches it to the
//! input's region instead, and `@path` tokens inside text attach the files
//! they name, the way `--<region>` flags always have.

use std::collections::BTreeMap;
use std::path::Path;

use leviath_core::mime::{InboundPart, MimeRegistry};
use leviath_runtime::spec::inputs::{InputDecl, InputSlot, InputType, RawInput};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};

use super::attach;

/// What the inputs typed on a command line read as.
#[derive(Debug, Default)]
pub(crate) struct ReadInputs {
    /// Each input's value, by name.
    pub(crate) values: BTreeMap<String, RawInput>,
    /// The files they named.
    pub(crate) parts: Vec<InboundPart>,
    /// `@path` tokens in text that named no file.
    pub(crate) unresolved: Vec<String>,
    /// Every input that did not read. Each is left out of `values`, and the
    /// rest are kept so the daemon can say what it makes of them too.
    pub(crate) issues: SpawnIssues,
}

/// One input as it was typed: its name, its text, and how it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Typed {
    /// The input's name.
    pub(crate) name: String,
    /// What was typed for it.
    pub(crate) text: String,
    /// The flag it came in, as an issue names it: `--input` or `--<name>`.
    pub(crate) flag: String,
}

impl Typed {
    /// An `--input name=value`. `Err` when there is no `=` or no name.
    pub(crate) fn from_input_flag(flag: &str) -> Result<Self, String> {
        match flag.split_once('=') {
            Some((name, text)) if !name.trim().is_empty() => Ok(Self {
                name: name.trim().to_string(),
                text: text.to_string(),
                flag: "--input".to_string(),
            }),
            _ => Err(format!(
                "--input '{flag}' is not name=value: write it as --input <name>=<value>"
            )),
        }
    }

    /// A `--<name> value`, the short way to give an input.
    pub(crate) fn from_named_flag(name: &str, text: &str) -> Self {
        Self {
            name: name.to_string(),
            text: text.to_string(),
            flag: format!("--{name}"),
        }
    }
}

/// Where an input's issues are: `inputs.<name>`.
fn at(name: &str) -> SpecPath {
    SpecPath::root().field("inputs").key(name)
}

/// Read every typed input against `decls`, collecting every problem: a name
/// nothing declares, a name given twice, a value that does not read as its
/// type, and a file that is not there. Files are read against `cwd`.
pub(crate) fn read_inputs(decls: &[InputDecl], typed: &[Typed], cwd: &Path) -> ReadInputs {
    let registry = attach::cli_registry();
    let mut out = ReadInputs::default();
    let mut issues = SpawnIssues::new();
    for given in typed {
        let Some(decl) = decls.iter().find(|d| d.name.as_str() == given.name) else {
            issues.push(
                SpawnIssue::new(
                    at(&given.name),
                    IssueCode::Unknown,
                    format!("{} names no input this run takes", given.flag),
                )
                .known(decls.iter().map(|d| &d.name)),
            );
            continue;
        };
        if out.values.contains_key(&given.name) {
            issues.push(SpawnIssue::new(
                at(&given.name),
                IssueCode::Invalid,
                "this input is given more than once",
            ));
            continue;
        }
        match read_value(decl, &given.text, cwd, &registry, &mut out) {
            Ok(Some(value)) => {
                out.values.insert(given.name.clone(), value);
            }
            Ok(None) => {}
            Err(Unread::Type(message)) => issues.push(
                SpawnIssue::new(at(&given.name), IssueCode::WrongType, message)
                    .expected(decl.ty.describe())
                    .got(format!("{:?}", given.text)),
            ),
            Err(Unread::File(message)) => issues.push(
                SpawnIssue::new(at(&given.name), IssueCode::Unresolvable, message)
                    .got(format!("{:?}", given.text))
                    .hint("name a file that exists, relative to where the command runs"),
            ),
        }
    }
    out.issues = issues;
    out
}

/// Why a value did not read.
enum Unread {
    /// It is not a value of the input's type.
    Type(String),
    /// A file it names could not be read.
    File(String),
}

/// The region a text input's files go to: the first region it fills, or
/// the region named after it.
fn region_of(decl: &InputDecl) -> String {
    decl.binds
        .iter()
        .find_map(|slot| match slot {
            InputSlot::Region(binding) => Some(binding.region.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| decl.name.to_string())
}

/// Read one value as `decl`'s type. `Ok(None)` for a text input whose whole
/// value was a file attached to its region, which leaves the input itself
/// unset.
fn read_value(
    decl: &InputDecl,
    text: &str,
    cwd: &Path,
    registry: &MimeRegistry,
    out: &mut ReadInputs,
) -> Result<Option<RawInput>, Unread> {
    match &decl.ty {
        InputType::Text { .. } => {
            let read = attach::read_region_input(&region_of(decl), text, cwd, registry)
                .map_err(|e| Unread::File(e.to_string()))?;
            out.parts.extend(read.parts);
            out.unresolved.extend(read.unresolved);
            Ok((!read.text.is_empty()).then_some(RawInput::Text(read.text)))
        }
        InputType::File { .. } => attach_file(text, cwd, out).map(Some),
        InputType::List { item, .. } if matches!(**item, InputType::File { .. }) => items(text)
            .into_iter()
            .map(|path| attach_file(path, cwd, out))
            .collect::<Result<_, _>>()
            .map(|files| Some(RawInput::List(files))),
        ty => typed_value(ty, text).map(Some).map_err(Unread::Type),
    }
}

/// Attach the file `text` names (`@path` or a bare path) and name it.
fn attach_file(text: &str, cwd: &Path, out: &mut ReadInputs) -> Result<RawInput, Unread> {
    let path = text.trim();
    let path = path.strip_prefix('@').unwrap_or(path);
    let part = attach::read_part(path, cwd).map_err(|e| Unread::File(e.to_string()))?;
    let name = part.name.clone();
    out.parts.push(part);
    Ok(RawInput::Text(name))
}

/// The comma-separated items of a list, trimmed. Nothing at all is no items.
fn items(text: &str) -> Vec<&str> {
    match text.trim().is_empty() {
        true => Vec::new(),
        false => text.split(',').map(str::trim).collect(),
    }
}

/// `text` as the wire carries a value of type `ty`.
pub(crate) fn typed_value(ty: &InputType, text: &str) -> Result<RawInput, String> {
    let word = text.trim();
    match ty {
        InputType::Bool => match word.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(RawInput::Bool(true)),
            "false" | "no" | "off" | "0" => Ok(RawInput::Bool(false)),
            _ => Err("this is not true or false".to_string()),
        },
        InputType::Int { .. } => word
            .parse()
            .map(RawInput::Int)
            .map_err(|_| "this is not a whole number".to_string()),
        InputType::Float { .. } => word
            .parse()
            .map(RawInput::Float)
            .map_err(|_| "this is not a number".to_string()),
        InputType::List { .. } if word.starts_with('[') => json(word)
            .filter(|v| matches!(v, RawInput::List(_)))
            .ok_or_else(|| "this is not a JSON array".to_string()),
        InputType::List { item, .. } => items(text)
            .into_iter()
            .map(|one| typed_value(item, one))
            .collect::<Result<_, _>>()
            .map(RawInput::List),
        InputType::Record { .. } => json(word)
            .filter(|v| matches!(v, RawInput::Record(_)))
            .ok_or_else(|| "this is not a JSON object such as {\"name\": \"value\"}".to_string()),
        _ => Ok(RawInput::Text(text.to_string())),
    }
}

/// `text` read as JSON, when it is.
fn json(text: &str) -> Option<RawInput> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
