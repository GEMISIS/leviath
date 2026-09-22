//! The `upsertMimeRow` and `deleteMimeRow` fields, and the input a row is
//! written from.

use async_graphql::{Context, InputObject, OneofObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::types::machine::MimeRow;

/// Tokens counted from the picture's area, as a write sends it.
#[derive(Debug, InputObject)]
pub(crate) struct PerPixelWrite {
    /// How many pixels one token buys.
    pub(crate) pixels_per_token: i32,
    /// The most one part may cost, and the answer when the dimensions cannot
    /// be read.
    pub(crate) max: Option<i32>,
}

/// How the tokens of a mime type are counted. Exactly one rate.
#[derive(Debug, OneofObject)]
pub(crate) enum MimeTokensWrite {
    /// Tokens per byte of the stored file.
    PerByte(f64),
    /// From the picture's area, capped.
    PerPixel(PerPixelWrite),
    /// Tokens per second of audio or video.
    PerSecond(i32),
    /// Tokens per page of a document.
    PerPage(i32),
    /// A flat charge, whatever the size.
    Fixed(i32),
}

impl MimeTokensWrite {
    /// The rule these rates describe.
    ///
    /// No refusal, unlike the JSON body the REST route reads: that one carries
    /// five nullable rates and has to be told "name exactly one", where this
    /// shape admits exactly one and the refusal happens in the parser.
    fn into_spec(self) -> crate::commands::mime_rows::TokenSpec {
        use crate::commands::mime_rows::TokenSpec;
        match self {
            Self::PerByte(rate) => TokenSpec::PerByte(rate),
            Self::PerPixel(pixels) => TokenSpec::PerPixel {
                divisor: i64::from(pixels.pixels_per_token),
                max: pixels.max.map(i64::from),
            },
            Self::PerSecond(rate) => TokenSpec::PerSecond(i64::from(rate)),
            Self::PerPage(rate) => TokenSpec::PerPage(i64::from(rate)),
            Self::Fixed(tokens) => TokenSpec::Fixed(i64::from(tokens)),
        }
    }
}

/// One row of the mime registry, as a write sends it.
///
/// Every field but the key is optional, because a row says only what it
/// changes: what a field leaves out stays as whatever broader row already
/// covers the type.
#[derive(Debug, InputObject)]
pub(crate) struct MimeRowWrite {
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
    pub(crate) tokens: Option<MimeTokensWrite>,
}

/// Which row to write.
#[derive(Debug, InputObject)]
pub(crate) struct UpsertMimeRowRequest {
    /// The row to write.
    pub(crate) row: MimeRowWrite,
}

/// The row as the registry now holds it.
#[derive(Debug, SimpleObject)]
pub(crate) struct UpsertMimeRowResult {
    /// The row, read back through the registry, so what comes back is what a
    /// run will resolve rather than what was sent.
    pub(crate) mime_row: MimeRow,
    /// Whether the row is new, rather than an update of one already there.
    pub(crate) is_new: bool,
}

/// Which row to take out.
#[derive(Debug, InputObject)]
pub(crate) struct DeleteMimeRowRequest {
    /// The row's key.
    pub(crate) mime_type: String,
}

/// What was taken out.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeleteMimeRowResult {
    /// The key the row was under.
    pub(crate) deleted_mime_type: String,
}

/// Add or update one row of the mime registry.
pub(crate) async fn upsert_mime_row(
    ctx: &Context<'_>,
    request: UpsertMimeRowRequest,
) -> async_graphql::Result<UpsertMimeRowResult> {
    let state = ctx.data_unchecked::<AppState>();
    let row = request.row;
    let tokens = row.tokens.map(MimeTokensWrite::into_spec);
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
    Ok(UpsertMimeRowResult {
        // The registry rather than the write: a row inherits from every broader
        // row above it, so what a run resolves for this type is not what the
        // write said on its own.
        mime_row: MimeRow::from_entry(super::super::super::blobs::mime_row_named(
            state,
            &written.mime_type,
        )),
        is_new: written.created,
    })
}

/// Remove a row from the mime registry.
///
/// A key nothing has a row for is a miss: the caller named a row, and there
/// was none to take out.
pub(crate) async fn delete_mime_row(
    request: DeleteMimeRowRequest,
) -> async_graphql::Result<DeleteMimeRowResult> {
    let removed = super::super::super::mime::remove_row_named(&request.mime_type).gql()?;
    match removed {
        true => Ok(DeleteMimeRowResult {
            deleted_mime_type: request.mime_type,
        }),
        false => Err(super::super::error::graphql_error(&ServeError::NotFound(
            format!("no row for '{}'", request.mime_type),
        ))),
    }
}
