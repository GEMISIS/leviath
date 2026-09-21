//! The `putMimeRow` and `deleteMimeRow` fields, and the input a row is
//! written from.

use super::super::super::core::error::ServeError;
use super::super::error::IntoGraphql;

/// One row of the mime registry, as a write sends it.
#[derive(async_graphql::InputObject)]
pub(crate) struct MimeRowInput {
    /// The type or pattern this row covers: `image/png`, or `image/*`.
    pub(crate) mime_type: String,
    /// The family providers key their encoders on.
    pub(crate) family: Option<String>,
    /// Whether the bytes are text, and so may travel inline.
    pub(crate) is_text: Option<bool>,
    /// Extensions that imply this type, without the dot.
    pub(crate) extensions: Option<Vec<String>>,
    /// A hex prefix that identifies the bytes.
    pub(crate) magic: Option<String>,
    /// What a consumer that cannot take the type sees in the part's place.
    pub(crate) stand_in: Option<String>,
    /// A script the bytes must pass to be stored as this type. An empty string
    /// lifts a check a broader row put on the type.
    pub(crate) check: Option<String>,
    /// How the tokens are counted.
    pub(crate) tokens: Option<MimeTokensInput>,
}

/// How the tokens of a mime type are counted. Name exactly one rate.
#[derive(async_graphql::InputObject)]
pub(crate) struct MimeTokensInput {
    /// Tokens per byte of the stored file.
    pub(crate) per_byte: Option<f64>,
    /// Pixels one token buys. Pair it with `max`.
    pub(crate) per_pixel: Option<i32>,
    /// The most one part may cost, and the answer when the dimensions are
    /// unknown. Only with `perPixel`.
    pub(crate) max: Option<i32>,
    /// Tokens per second of audio or video.
    pub(crate) per_second: Option<i32>,
    /// Tokens per page of a document.
    pub(crate) per_page: Option<i32>,
    /// A flat charge, whatever the size.
    pub(crate) fixed: Option<i32>,
}

impl MimeTokensInput {
    /// The rule these rates describe, or why they describe none.
    ///
    /// Through the same reader the REST route uses, so "exactly one rate" means
    /// the same thing on both surfaces.
    fn into_spec(self) -> Result<crate::commands::mime_rows::TokenSpec, ServeError> {
        super::super::super::mime::TokenRuleReq {
            per_byte: self.per_byte,
            per_pixel: self.per_pixel.map(i64::from),
            per_second: self.per_second.map(i64::from),
            per_page: self.per_page.map(i64::from),
            fixed: self.fixed.map(i64::from),
            max: self.max.map(i64::from),
        }
        .into_spec()
        .map_err(ServeError::BadRequest)
    }
}

/// What writing a mime row did.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct MimeRowWritten {
    /// The row's key.
    pub(crate) mime_type: String,
    /// True when the row is new, false when an existing one was updated.
    pub(crate) created: bool,
}

/// Add or update one row of the mime registry.
///
/// Every field but the key is optional, because a row says only what it
/// changes: what a field leaves out stays as whatever broader row already
/// covers the type.
pub(crate) async fn put_mime_row(row: MimeRowInput) -> async_graphql::Result<MimeRowWritten> {
    let tokens = match row.tokens {
        None => None,
        Some(rates) => Some(rates.into_spec().gql()?),
    };
    let written = super::super::super::mime::write_edit(
        &row.mime_type,
        crate::commands::mime_rows::RowEdit {
            family: row.family,
            text: row.is_text,
            tokens,
            extensions: row.extensions,
            magic: row.magic,
            stand_in: row.stand_in,
            check: row.check,
        },
    )
    .gql()?;
    Ok(MimeRowWritten {
        mime_type: written.mime_type,
        created: written.created,
    })
}

/// Remove a row from the mime registry.
///
/// False when there was no such row, which is a fact about the registry
/// rather than a failed request.
pub(crate) async fn delete_mime_row(mime_type: String) -> async_graphql::Result<bool> {
    super::super::super::mime::remove_row_named(&mime_type).gql()
}
