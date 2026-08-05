//! Built-in well-formedness checks for common output formats.
//!
//! These answer one question: **is the submission actually the format it claims
//! to be?** They do not answer "does it have the right shape", which is JSON
//! Schema's job (for JSON) or an agent's own Rhai validator's job (for anything
//! else).
//!
//! The distinction is the whole reason this module is small. Checking that a
//! document parses is one call into a parser. Checking that it matches an XSD,
//! a JSON Schema, or a GraphQL schema means owning that format's schema
//! language, which is exactly the per-format weight the output system is built
//! to avoid.
//!
//! What this catches is the failure that actually happens: the model wraps its
//! answer in ``` fences, adds a sentence of preamble, or hands back JSON when
//! the stage asked for XML. All three are a parse error, and all three are worth
//! bouncing back for a retry.
//!
//! A format with no entry here validates nothing, which is the honest outcome:
//! the label is opaque by design, and most labels are not formats this crate has
//! ever heard of.

/// Whether a built-in check exists for `format`.
///
/// Matched case-insensitively on the whole label, so `"JSON"` and `"json"` are
/// the same check and `"json-lines"` is neither. A near-miss deliberately gets
/// no validation rather than the wrong one.
pub fn has_builtin(format: &str) -> bool {
    checker_for(format).is_some()
}

/// Every format this crate can check, for documentation and diagnostics.
pub const BUILTIN_FORMATS: &[&str] = &["json", "xml", "yaml", "csv", "toml"];

/// A well-formedness check: `Ok` when the content parses, `Err` with a reason
/// the agent can act on when it does not.
type Checker = fn(&str) -> Result<(), String>;

/// The checker for `format`, if there is one.
fn checker_for(format: &str) -> Option<Checker> {
    // Trimmed and lowercased, because a label is written by a person and
    // `"JSON "` means JSON. Nothing cleverer: a label this does not recognize is
    // simply not validated.
    match format.trim().to_ascii_lowercase().as_str() {
        "json" => Some(check_json),
        "xml" => Some(check_xml),
        "yaml" | "yml" => Some(check_yaml),
        "csv" => Some(check_csv),
        "toml" => Some(check_toml),
        _ => None,
    }
}

/// Check `content` against the built-in for `format`.
///
/// `Ok(())` when it parses, when the format has no built-in, or when `format` is
/// absent. `Err` carries a message written for the agent to act on.
pub fn check(format: Option<&str>, content: &str) -> Result<(), String> {
    let Some(checker) = format.and_then(checker_for) else {
        return Ok(());
    };
    checker(content)
}

fn check_json(content: &str) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(content)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn check_toml(content: &str) -> Result<(), String> {
    content
        .parse::<toml::Table>()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn check_yaml(content: &str) -> Result<(), String> {
    yaml_rust2::YamlLoader::load_from_str(content)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Read every XML event to the end, tracking element depth.
///
/// The reader reports a *mismatched* closing tag on its own, but not one that
/// never arrives: `<a>` with no `</a>` reaches EOF without complaint. So depth
/// is counted here, and anything still open at the end is the error.
///
/// A document with no elements at all is refused too. Prose parses as a single
/// text event and would otherwise pass as "valid XML", which is the wrong answer
/// for a stage that asked for XML and got a paragraph.
fn check_xml(content: &str) -> Result<(), String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(content);
    let mut buf = Vec::new();
    let mut depth: i64 = 0;
    let mut saw_element = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(_)) => {
                saw_element = true;
                depth += 1;
            }
            Ok(Event::Empty(_)) => saw_element = true,
            Ok(Event::End(_)) => depth -= 1,
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        buf.clear();
    }
    if !saw_element {
        return Err("no XML elements found; this looks like plain text".to_string());
    }
    match depth {
        0 => Ok(()),
        open => Err(format!(
            "{open} element(s) left unclosed at the end of the document"
        )),
    }
}

/// Read every record, which surfaces an unbalanced quote or a ragged row.
///
/// Ragged rows are the point: a CSV whose columns drift is the failure a
/// consumer actually hits, and the reader reports it as an error rather than
/// quietly yielding short records.
fn check_csv(content: &str) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("the CSV is empty".to_string());
    }
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(content.as_bytes());
    for record in reader.records() {
        record.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
