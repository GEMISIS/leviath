//! Paged listings.
//!
//! The REST routes hand out a `Page` envelope with a `next_cursor`. GraphQL
//! says the same thing in the shape its clients already generate code for:
//! edges carrying a cursor each, and a `pageInfo` saying whether another page
//! follows. Both are the same keyset walk underneath, and the cursor tokens
//! are interchangeable between the two surfaces.

use async_graphql::SimpleObject;

use super::scalars::{Cursor, Timestamp};
use super::types::run::Run;

/// Keyset page state.
#[derive(Debug, SimpleObject)]
pub(crate) struct PageInfo {
    /// Whether another page follows this one.
    pub(crate) has_next_page: bool,
    /// Pass as `after` to get it.
    pub(crate) end_cursor: Option<Cursor>,
}

/// Where a search matched, and enough text to show a person why.
///
/// The part of search a browser cannot do: the client never holds a run's
/// transcript, so without this a deep match is an unexplained result.
#[derive(Debug, SimpleObject)]
pub(crate) struct Highlight {
    /// What matched: a run field name, `metadata.<key>`, `modified_files`,
    /// `context.<region>`, `logs.output`, `logs.operational`, or
    /// `journal.tool.<tool_name>`.
    pub(crate) field: String,
    /// The matching text, with a little either side.
    pub(crate) snippet: String,
    /// Which stage the match came from, for the sources that have one.
    pub(crate) stage: Option<i32>,
}

/// One run with its page cursor and its match highlights.
#[derive(SimpleObject)]
pub(crate) struct RunEdge {
    /// The run.
    pub(crate) node: Run,
    /// Cursor for this edge.
    pub(crate) cursor: Cursor,
    /// Why this run matched a search; empty when there was no search.
    pub(crate) highlights: Vec<Highlight>,
}

/// A keyset-paged run listing.
#[derive(SimpleObject)]
pub(crate) struct RunConnection {
    /// The runs on this page.
    pub(crate) edges: Vec<RunEdge>,
    /// Keyset page state.
    pub(crate) page_info: PageInfo,
    /// How many runs matched. Null when `scanTruncated` is true: a count from
    /// a partial scan is not a fact.
    pub(crate) total: Option<i32>,
    /// True when the search gave up before covering the store.
    pub(crate) scan_truncated: bool,
    /// Ids from an `ids` fetch that name no run here. Never fails the request.
    pub(crate) missing: Vec<String>,
    /// Daemon time when the page was built. Pass it back as `since` to poll.
    pub(crate) server_time: Timestamp,
}
