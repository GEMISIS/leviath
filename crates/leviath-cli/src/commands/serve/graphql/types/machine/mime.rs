//! One row of the mime registry, as the schema describes it.

use async_graphql::SimpleObject;

/// One row of the mime registry.
#[derive(Debug, SimpleObject)]
pub(crate) struct MimeRow {
    /// The row's key: a type, or a pattern such as `image/*`.
    pub(crate) mime_type: String,
    /// Where the row came from: `builtin`, `config`, or a blueprint's name.
    pub(crate) source: String,
    /// The family the type resolves to, which is what providers key their
    /// encoders on.
    pub(crate) family: Option<String>,
    /// Whether the bytes are text, and so may travel inline.
    pub(crate) is_text: Option<bool>,
    /// The extensions this type is known by.
    pub(crate) extensions: Vec<String>,
}
