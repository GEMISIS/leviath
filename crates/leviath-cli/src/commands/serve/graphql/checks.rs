//! The three pure checks, and what they answer with.
//!
//! Each of these takes text and gives a verdict: nothing is written, nothing is
//! dialled, nothing is run. They are `Query` fields for that reason, so a
//! caching client, a `GET`-only proxy or a reader with no write access can use
//! them. The types live here rather than beside the writes they usually precede,
//! because a check is not half of a mutation.

use async_graphql::SimpleObject;
use leviath_graphql_derive::mirror;

/// What a validation found.
///
/// `valid` is the verdict; the lists say why. A manifest can be valid and still
/// carry warnings, which is the common case for a blueprint that works but names
/// something the engine has retired.
///
/// Read and never filtered: a verdict is what one call answered about one piece
/// of text, so there is no listing of them to select from.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ValidationReport {
    /// Whether the manifest would install and run.
    pub(crate) valid: bool,
    /// What makes it invalid. Empty when it is.
    pub(crate) errors: Vec<String>,
    /// What is worth knowing about it anyway.
    pub(crate) warnings: Vec<String>,
}

/// Whether a key looks like one of that provider's.
#[mirror(no_filter)]
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct KeyVerdict {
    /// Whether the format is right. Not whether the key works: nothing was
    /// dialled.
    pub(crate) valid: bool,
    /// What is wrong with it, when something is.
    pub(crate) message: Option<String>,
}

/// Whether a script compiles.
#[mirror(no_filter)]
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct ScriptVerdict {
    /// Whether the compiler for this kind accepts the text.
    pub(crate) valid: bool,
    /// The compiler's complaint, when it does not.
    pub(crate) error: Option<String>,
}
