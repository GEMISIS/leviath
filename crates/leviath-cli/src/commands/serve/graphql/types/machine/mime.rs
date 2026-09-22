//! One row of the mime registry, as the schema describes it.
//!
//! One type for every row, whether the operator wrote it in `mime_types.toml`,
//! a blueprint ships it, or it is compiled in. A row is the same thing in all
//! three places, and two types for it meant a client rendering a blueprint's
//! rows could not reuse the view it already had.

use async_graphql::{Enum, SimpleObject, Union};
use leviath_graphql_derive::mirror;

/// Tokens counted from the stored bytes.
#[mirror]
#[derive(Debug, SimpleObject)]
#[graphql(name = "PerByteOutput")]
pub(crate) struct PerByte {
    /// Tokens per byte of the stored file. `0.25` is the text rule.
    pub(crate) tokens_per_byte: f64,
}

/// Tokens counted from the picture's area, capped.
#[mirror]
#[derive(Debug, SimpleObject)]
#[graphql(name = "PerPixelOutput")]
pub(crate) struct PerPixel {
    /// How many pixels one token buys.
    pub(crate) pixels_per_token: i32,
    /// The most one part may cost, and the answer when the dimensions cannot
    /// be read.
    pub(crate) max: i32,
}

/// Tokens counted from the running time.
#[mirror]
#[derive(Debug, SimpleObject)]
#[graphql(name = "PerSecondOutput")]
pub(crate) struct PerSecond {
    /// Tokens per second of audio or video.
    pub(crate) tokens_per_second: i32,
}

/// Tokens counted from the page count.
#[mirror]
#[derive(Debug, SimpleObject)]
#[graphql(name = "PerPageOutput")]
pub(crate) struct PerPage {
    /// Tokens per page of a document.
    pub(crate) tokens_per_page: i32,
}

/// A flat charge, whatever the size.
#[mirror]
#[derive(Debug, SimpleObject)]
#[graphql(name = "FixedOutput")]
pub(crate) struct Fixed {
    /// What one part of this type costs.
    pub(crate) tokens: i32,
}

/// How a part's tokens are estimated.
///
/// A union rather than one object with five nullable rates: a row names
/// exactly one rate, and a bag of nullables would admit combinations no
/// registry can hold.
#[mirror]
#[derive(Debug, Union)]
pub(crate) enum MimeTokenRule {
    /// From the stored bytes.
    Bytes(PerByte),
    /// From the picture's area.
    Pixels(PerPixel),
    /// From the running time.
    Seconds(PerSecond),
    /// From the page count.
    Pages(PerPage),
    /// A flat charge.
    Flat(Fixed),
}

impl From<&leviath_core::mime::TokenRule> for MimeTokenRule {
    fn from(rule: &leviath_core::mime::TokenRule) -> Self {
        use leviath_core::mime::TokenRule as Core;
        match rule {
            Core::PerByte(rate) => Self::Bytes(PerByte {
                tokens_per_byte: *rate,
            }),
            Core::PerPixel { divisor, max } => Self::Pixels(PerPixel {
                pixels_per_token: count(*divisor),
                max: size(*max),
            }),
            Core::PerSecond(rate) => Self::Seconds(PerSecond {
                tokens_per_second: count(*rate),
            }),
            Core::PerPage(rate) => Self::Pages(PerPage {
                tokens_per_page: size(*rate),
            }),
            Core::Fixed(tokens) => Self::Flat(Fixed {
                tokens: size(*tokens),
            }),
        }
    }
}

/// Narrow a rate to the 32 bits GraphQL's `Int` carries.
fn count(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// The same, for a count the registry holds as a `usize`.
fn size(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Where a mime row came from.
///
/// Three layers, narrowest last: this build ships a table, the operator's
/// config writes over it, and a blueprint's own rows write over that for its
/// runs. `blueprintName` says which blueprint, for the third.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum MimeRowOrigin {
    /// This build's compiled table.
    Builtin,
    /// The operator's config file.
    Config,
    /// A blueprint's own `[mime_types]`, named in `blueprintName`.
    Blueprint,
}

/// One row of the mime registry.
///
/// A row says only what it sets: what a field leaves null is whatever broader
/// row already covers the type, which is why almost every field is nullable.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct MimeRow {
    /// The row's key: a type, or a pattern such as `image/*`.
    #[filter(orderable)]
    pub(crate) mime_type: String,
    /// Which layer wrote the row.
    pub(crate) origin: MimeRowOrigin,
    /// The blueprint whose own rows this came from. Null for the two layers
    /// that belong to the machine rather than to one blueprint.
    #[filter(orderable)]
    pub(crate) blueprint_name: Option<String>,
    /// The family the type resolves to, which is what providers key their
    /// encoders on.
    #[filter(orderable)]
    pub(crate) family: Option<String>,
    /// Whether the bytes are text, and so may travel inline.
    pub(crate) is_text: Option<bool>,
    /// How the part's tokens are estimated.
    pub(crate) tokens: Option<MimeTokenRule>,
    /// The extensions this type is known by, without the dot.
    pub(crate) extensions: Vec<String>,
    /// A hex prefix that identifies the bytes, two digits per byte, with `??`
    /// for a byte that may be anything.
    pub(crate) magic: Option<String>,
    /// What a consumer that cannot take this type sees in the part's place.
    pub(crate) stand_in: Option<String>,
    /// A script the bytes must pass before they are stored as this type.
    pub(crate) check: Option<String>,
}

impl super::super::super::connection::Paged for MimeRow {
    const NAME: &'static str = "MimeRow";
}

impl MimeRow {
    /// Read the `source` word a registry row carries as the layer that wrote
    /// it, and the blueprint's name where there is one.
    ///
    /// The machine's own layers name themselves: `builtin` for the compiled
    /// table, `provider:<name>` for the rows a Rhai provider ships under it,
    /// and the operator's two config layers, which are the `[mime_types]` table
    /// (`config`) and the file beside it (`mime_types.toml`). Anything else is
    /// a blueprint, and the word is its name.
    pub(crate) fn origin_of(source: String) -> (MimeRowOrigin, Option<String>) {
        match source.as_str() {
            "config" | crate::config::MIME_TYPES_FILE => (MimeRowOrigin::Config, None),
            "builtin" => (MimeRowOrigin::Builtin, None),
            // A provider's own types ship with its script rather than with a
            // blueprint, and sit under the compiled table for the same reason:
            // the operator's file and a blueprint both still win.
            other if other.starts_with("provider:") => (MimeRowOrigin::Builtin, None),
            _ => (MimeRowOrigin::Blueprint, Some(source)),
        }
    }

    /// Describe one row of the effective registry.
    ///
    /// The registry is what a run resolves, so this is the reading both the
    /// listing and a write's answer hand back: a row inherits from every
    /// broader row above it, and the row as written is not that.
    pub(crate) fn from_entry(entry: super::super::super::super::blobs::MimeTypeEntry) -> Self {
        let (origin, blueprint_name) = Self::origin_of(entry.source);
        Self {
            mime_type: entry.mime_type,
            origin,
            blueprint_name,
            family: entry.family,
            is_text: entry.text,
            tokens: entry.tokens.as_ref().map(MimeTokenRule::from),
            extensions: entry.extensions.unwrap_or_default(),
            magic: entry.magic,
            stand_in: entry.stand_in,
            check: entry.check,
        }
    }

    /// Read the `[mime_types]` table a blueprint carries.
    ///
    /// Every row is that blueprint's own, which is what `origin` says and what
    /// `blueprint` names. A row that will not deserialize is left out rather
    /// than reported as an empty row: the manifest loader refuses such a
    /// blueprint, so reaching one here means the file moved underneath an
    /// installed run, and an empty row would read as a row that sets nothing.
    pub(crate) fn from_table(table: &toml::Table, blueprint: &str) -> Vec<Self> {
        let mut rows: Vec<Self> = table
            .iter()
            .filter_map(|(mime_type, value)| {
                let row: leviath_core::mime::registry::MimeRow = value.clone().try_into().ok()?;
                Some(Self {
                    mime_type: mime_type.clone(),
                    origin: MimeRowOrigin::Blueprint,
                    blueprint_name: Some(blueprint.to_string()),
                    family: row.family,
                    is_text: row.text,
                    tokens: row.tokens.as_ref().map(MimeTokenRule::from),
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
