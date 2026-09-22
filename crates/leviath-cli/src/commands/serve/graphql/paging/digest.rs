//! The canonical text a filter digests to, and the size limits the same walk
//! enforces.
//!
//! A cursor carries a digest of the filter it was minted for, so changing the
//! filter mid-walk is a refusal rather than a page of quietly wrong rows. What
//! goes into that digest has to depend on what the client *said*, and on
//! nothing else: a `{:?}` of the compiled matcher changes whenever a field is
//! added to the filter type, and every cursor in flight dies with it.
//!
//! So the digest is taken over the filter's own GraphQL value, rendered here:
//!
//! - nulls and objects that end up empty are dropped, so leaving a field out
//!   and passing it as null are the same filter, which they are;
//! - fields keep their declaration order, so two spellings of one filter
//!   render the same way without a sort;
//! - **an empty filter renders as the empty string**, and an empty part is
//!   never appended to a digest. That is what keeps an unfiltered GraphQL
//!   listing digesting exactly as the unfiltered REST one does, so a cursor
//!   from either surface resumes on the other.
//!
//! `MAX_DEPTH` and `MAX_COMPLEXITY` only count selection sets, so a filter
//! could nest `and`/`or` as deep as a parser would take it. The same walk that
//! renders the value counts it, and refuses one that is too deep or too large
//! before anything is matched against it.

use async_graphql::{InputType, Value};

use crate::commands::serve::core::error::ServeError;

/// How deeply one filter may nest.
///
/// A real filter is two or three levels: a relation, a field, its operators.
/// Sixteen is far past anything a person writes and far short of what costs
/// anything to walk.
pub(crate) const MAX_FILTER_DEPTH: usize = 16;

/// How many values one filter may hold, counting every operator, every list
/// element and every nested object.
pub(crate) const MAX_FILTER_NODES: usize = 512;

/// Render a filter input as the text its cursor digest is taken over.
///
/// An empty filter gives an empty string, which the caller appends as nothing
/// at all rather than as an empty part.
pub(crate) fn canonical<T: InputType>(filter: &T) -> Result<String, ServeError> {
    canonical_value(&filter.to_value())
}

/// Render one GraphQL value the same way, for a caller that already holds one.
pub(crate) fn canonical_value(value: &Value) -> Result<String, ServeError> {
    Walk { nodes: 0 }.render(value, 1)
}

/// The rendering pass, carrying the node count it is also enforcing.
struct Walk {
    /// How many values have been rendered so far.
    nodes: usize,
}

impl Walk {
    /// Render one value at `depth`, counting it against both limits.
    fn render(&mut self, value: &Value, depth: usize) -> Result<String, ServeError> {
        self.nodes += 1;
        if self.nodes > MAX_FILTER_NODES {
            return Err(ServeError::BadRequest(format!(
                "Filter holds more than {MAX_FILTER_NODES} values; split the query"
            )));
        }
        if depth > MAX_FILTER_DEPTH {
            return Err(ServeError::BadRequest(format!(
                "Filter nests more than {MAX_FILTER_DEPTH} levels deep; flatten it"
            )));
        }
        Ok(match value {
            // An absent field and a null one are the same filter, so both
            // render as nothing and the object above drops them.
            Value::Null => String::new(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => quoted(s),
            Value::Boolean(b) => b.to_string(),
            Value::Enum(name) => name.to_string(),
            Value::Binary(bytes) => hex::encode(bytes),
            Value::List(items) => {
                let mut parts = Vec::with_capacity(items.len());
                for item in items {
                    let rendered = self.render(item, depth + 1)?;
                    // A null holds its place in a list, where position is part
                    // of the value, so `[null]` cannot render as `[]`.
                    parts.push(match rendered.is_empty() {
                        true => "null".to_string(),
                        false => rendered,
                    });
                }
                format!("[{}]", parts.join(","))
            }
            Value::Object(fields) => {
                let mut parts = Vec::with_capacity(fields.len());
                for (name, field) in fields {
                    let rendered = self.render(field, depth + 1)?;
                    if rendered.is_empty() {
                        continue;
                    }
                    parts.push(format!("{name}:{rendered}"));
                }
                match parts.is_empty() {
                    true => String::new(),
                    false => format!("{{{}}}", parts.join(",")),
                }
            }
        })
    }
}

/// Quote a string so its content cannot be confused with the punctuation
/// around it: a name of `a,b` must not render like two fields.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
#[path = "digest_tests.rs"]
mod tests;
