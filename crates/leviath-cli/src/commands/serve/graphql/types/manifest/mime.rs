//! The mime rows a blueprint ships with itself.
//!
//! A blueprint that works in a file type the machine has never heard of carries
//! the row that describes it, so installing the blueprint is all it takes.

use async_graphql::SimpleObject;

use super::count;

/// How a part's tokens are estimated. Exactly one rate is set.
#[derive(Debug, SimpleObject)]
pub(crate) struct MimeTokenRates {
    /// Tokens per byte of the stored file.
    pub(crate) per_byte: Option<f64>,
    /// Pixels one token buys, paired with `max`.
    pub(crate) per_pixel: Option<i32>,
    /// The most one part may cost, and the answer when the dimensions are
    /// unknown.
    pub(crate) max: Option<i32>,
    /// Tokens per second of audio or video.
    pub(crate) per_second: Option<i32>,
    /// Tokens per page of a document.
    pub(crate) per_page: Option<i32>,
    /// A flat charge, whatever the size.
    pub(crate) fixed: Option<i32>,
}

impl From<&leviath_core::mime::TokenRule> for MimeTokenRates {
    fn from(rule: &leviath_core::mime::TokenRule) -> Self {
        use leviath_core::mime::TokenRule as Core;
        let mut rates = Self {
            per_byte: None,
            per_pixel: None,
            max: None,
            per_second: None,
            per_page: None,
            fixed: None,
        };
        match rule {
            Core::PerByte(rate) => rates.per_byte = Some(*rate),
            Core::PerPixel { divisor, max } => {
                rates.per_pixel = Some(i32::try_from(*divisor).unwrap_or(i32::MAX));
                rates.max = Some(count(*max));
            }
            Core::PerSecond(rate) => {
                rates.per_second = Some(i32::try_from(*rate).unwrap_or(i32::MAX));
            }
            Core::PerPage(rate) => rates.per_page = Some(count(*rate)),
            Core::Fixed(tokens) => rates.fixed = Some(count(*tokens)),
        }
        rates
    }
}

/// One row of the mime registry, as a blueprint writes it.
///
/// Every field but the key is optional, because a row says only what it
/// changes: a row that names a family and nothing else leaves the rest to
/// whatever broader row already covers the type.
#[derive(Debug, SimpleObject)]
pub(crate) struct BlueprintMimeRow {
    /// The type or pattern this row covers: `image/png`, or `image/*`.
    pub(crate) mime_type: String,
    /// What providers key their encoders on.
    pub(crate) family: Option<String>,
    /// Whether the bytes are text, and so may travel inline and reach any model
    /// as text.
    pub(crate) is_text: Option<bool>,
    /// How the part's tokens are estimated.
    pub(crate) tokens: Option<MimeTokenRates>,
    /// Extensions that imply this type, without the dot.
    pub(crate) extensions: Vec<String>,
    /// A hex prefix that identifies the bytes.
    pub(crate) magic: Option<String>,
    /// What a consumer that cannot take this type sees in the part's place.
    pub(crate) stand_in: Option<String>,
    /// A script the bytes must pass before they are stored as this type. An
    /// empty string lifts a check a broader row put on the type.
    pub(crate) check: Option<String>,
}

impl BlueprintMimeRow {
    /// Read the `[mime_types]` table a blueprint carries.
    ///
    /// A row that will not deserialize is left out rather than reported as an
    /// empty row: the manifest loader refuses such a blueprint, so reaching one
    /// here means the file moved underneath an installed run, and an empty row
    /// would read as a row that sets nothing.
    pub(crate) fn from_table(table: &toml::Table) -> Vec<Self> {
        let mut rows: Vec<Self> = table
            .iter()
            .filter_map(|(mime_type, value)| {
                let row: leviath_core::mime::registry::MimeRow = value.clone().try_into().ok()?;
                Some(Self {
                    mime_type: mime_type.clone(),
                    family: row.family,
                    is_text: row.text,
                    tokens: row.tokens.as_ref().map(MimeTokenRates::from),
                    extensions: row.extensions.unwrap_or_default(),
                    magic: row.magic,
                    stand_in: row.stand_in,
                    check: row.check,
                })
            })
            .collect();
        rows.sort_by(|a, b| a.mime_type.cmp(&b.mime_type));
        rows
    }
}
