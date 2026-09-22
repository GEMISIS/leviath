//! The small types and helpers `Run`'s own resolvers lean on: token and spend
//! roll-ups, the `logs` argument shapes, the bounded per-run connections
//! (`stages`, `blobs`, `artifacts`) share, and one point of `contextHistory`.
//!
//! Split out of `run.rs` on size alone - none of this is a concern of its
//! own, it is what the big `#[Object] impl Run` needed room to stay under the
//! file's line cap.

use async_graphql::{Enum, OneofObject, SimpleObject};
use leviath_graphql_derive::mirror;

use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::error::ServeError;
use crate::commands::serve::core::history;
use crate::commands::serve::cursor;
use crate::commands::serve::graphql::connection::{
    Connection, Paged, PositionPage, PositionQuery, Total,
};
use crate::commands::serve::graphql::error::IntoGraphql;
use crate::commands::serve::graphql::filter::{Filterable, MatchCx, OrderField, Orderable, Sifted};
use crate::commands::serve::graphql::paging::digest::canonical;
use crate::commands::serve::graphql::paging::order::{Order, Term};
use crate::commands::serve::graphql::paging::page::page;
use crate::commands::serve::graphql::paging::walk::walk;
use crate::commands::serve::graphql::scalars::{BigInt, Cursor, Decimal, Timestamp};
use crate::commands::serve::graphql::types::run_detail::ContextWindow;
use crate::commands::serve::types::AppState;

/// Which stage `logs` reads. Omitted entirely means the stage the run is on
/// now.
#[derive(Debug, OneofObject)]
pub(crate) enum LogStageOptions {
    /// One stage by index.
    Index(i32),
    /// Every stage's logs, in order, instead of one stage's.
    All(bool),
}

/// Which stream `logs` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum LogStream {
    /// What the stage's process wrote.
    Output,
    /// The daemon's own record of running it.
    Operational,
}

/// Token counts for a run, a stage, or a subtree roll-up.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct TokenUsage {
    /// Input tokens, cached ones included.
    pub(crate) prompt_tokens: BigInt,
    /// Output tokens.
    pub(crate) completion_tokens: BigInt,
    /// Counted within `promptTokens`, not on top of it. Do not add them twice.
    pub(crate) cached_tokens: BigInt,
    /// Tokens written to the provider's cache.
    pub(crate) cache_write_tokens: BigInt,
}

/// Spend for a run, a stage, or a subtree roll-up.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct CostBreakdown {
    /// Null is unknown, never free: some call in the run went unpriced.
    pub(crate) cost_usd: Option<Decimal>,
    /// The priced subtotal, kept even while `costUsd` is null.
    pub(crate) cost_priced_usd: Decimal,
    /// True when every priced call carried the provider's own figure.
    pub(crate) cost_is_exact: bool,
    /// Calls no provider priced.
    pub(crate) unpriced_calls: i32,
}

/// Elapsed working time: banked spans plus the one in progress.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct WorkingClock {
    /// Seconds banked by spans that have ended.
    pub(crate) banked_secs: i32,
    /// When the span in progress began; null while the clock is stopped.
    pub(crate) since: Option<Timestamp>,
}

/// One caller-supplied metadata entry on a run.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct MetadataEntry {
    /// The key.
    pub(crate) key: String,
    /// The value. Always a string.
    pub(crate) value: String,
}

/// How deep and how wide a run's sub-agent tree is.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunTreeStatus {
    /// Token roll-up over the whole subtree, this run included.
    pub(crate) rollup: TokenUsage,
    /// The deepest nesting below this run.
    pub(crate) depth: i32,
    /// How many runs are below it, at any depth.
    pub(crate) descendant_count: i32,
}

/// How much of a log stream is read when the client does not say.
pub(crate) const DEFAULT_LOG_TAIL_BYTES: u64 = 32 * 1024;

/// The most one request reads from each stream.
///
/// `allStages` multiplies whatever is asked for by the stage count, so the cap
/// is the server's rather than the client's.
pub(crate) const MAX_LOG_TAIL_BYTES: u64 = 1024 * 1024;

/// Read the tail of a run's logs, as the `logs` resolver's arguments ask for
/// it.
///
/// The three arguments are translated here rather than in the resolver so that
/// the refusals a negative number earns are one reading: a stage index and a
/// window are both counts, and neither has a meaning below zero.
pub(crate) async fn tail_logs(
    run_id: &str,
    stage: Option<LogStageOptions>,
    stream: LogStream,
    tail_bytes: Option<i32>,
) -> async_graphql::Result<String> {
    let selector = match stage {
        None => crate::runstate::StageSelector::Current,
        Some(LogStageOptions::All(_)) => crate::runstate::StageSelector::All,
        Some(LogStageOptions::Index(index)) => crate::runstate::StageSelector::Index(
            usize::try_from(index)
                .map_err(|_| {
                    crate::commands::serve::core::error::ServeError::BadRequest(
                        "`stage.index` cannot be negative".to_string(),
                    )
                })
                .gql()?,
        ),
    };
    let stream = match stream {
        LogStream::Operational => crate::runstate::LogStream::Operational,
        LogStream::Output => crate::runstate::LogStream::Output,
    };
    let bytes = match tail_bytes {
        None => DEFAULT_LOG_TAIL_BYTES,
        Some(asked) => u64::try_from(asked)
            .map_err(|_| {
                crate::commands::serve::core::error::ServeError::BadRequest(
                    "`tailBytes` cannot be negative".to_string(),
                )
            })
            .gql()?
            .min(MAX_LOG_TAIL_BYTES),
    };
    let run_id = run_id.to_string();
    Ok(crate::commands::serve::blocking::blocking(move || {
        crate::runstate::tail_run_logs(&run_id, selector, stream, bytes)
    })
    .await)
}

/// Narrow a daemon counter to the 32 bits GraphQL's `Int` carries.
///
/// These are iteration and tool-call counters, which a run reaches in the
/// thousands at most. Saturating rather than wrapping: if one ever did run
/// away, a client should read an implausible ceiling rather than a small
/// number that looks fine.
pub(crate) fn as_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// The most one page of `stages`, `blobs` or `artifacts` may hold.
///
/// Each of these is bounded by the run's own record - a blueprint's stage
/// count, the parts one run stored, the files one submission named - so this
/// cap is about a client that meant to page and did not, the same way the
/// blueprint catalogue's is.
const BOUNDED_PAGE_CAP: usize = 200;

/// The page-shaped half of a [`bounded_page`] call, bundled apart from what to
/// walk and how to filter and order it.
pub(crate) struct BoundedPageArgs {
    /// Page size, as the client asked for it.
    pub(crate) first: i32,
    /// The previous page's cursor, if this is not the first.
    pub(crate) after: Option<Cursor>,
}

/// Page a list already read whole from one of a run's own bounded records:
/// its stages, its stored parts, its submitted files. Each is read by one file
/// (or one directory listing), so counting it is free, and each still takes
/// the filter, `orderBy` and keyset cursor every listing in this schema takes
/// - for the one blueprint or the one submission that has hundreds of them.
pub(crate) async fn bounded_page<T, F>(
    kind: &str,
    run_id: &str,
    items: Vec<T>,
    filter: Option<T::Filter>,
    terms: Vec<Term<F>>,
    page_args: BoundedPageArgs,
    id: fn(&T) -> String,
) -> async_graphql::Result<Connection<T>>
where
    T: Paged
        + async_graphql::OutputType
        + Filterable
        + Orderable<MatchCx<'static>, Field = F>
        + Send
        + Sync
        + 'static,
    T::Filter: Default + async_graphql::InputType + Send + Sync,
    F: OrderField + Send + Sync + 'static,
{
    let BoundedPageArgs { first, after } = page_args;
    let limit = page(first, BOUNDED_PAGE_CAP, "the page cap").gql()?;
    let filter = filter.unwrap_or_default();
    let rendered = canonical(&filter).gql()?;
    let digest = cursor::filter_digest(&[kind, run_id, rendered.as_str()]);
    let order = Order::new(terms);
    let resume = match after {
        None => None,
        Some(token) => Some(order.decode(&token.0, &digest).gql()?),
    };
    let cx = MatchCx::at(leviath_core::duration::now_secs());
    let sift = Sifted::new(filter, cx, order, id);
    let mut walked = walk(sift, items, resume.as_ref(), limit).await;
    let results = std::mem::take(&mut walked.items);
    let cursor = walked
        .next
        .clone()
        .map(|position| Cursor(walked.sift().order().encode(&digest, &position)));
    Ok(Connection::plain(
        results,
        cursor,
        Total::lazy(async move { walked.total().await }),
    ))
}

/// A signed link to one of a run's byte routes.
///
/// One place mints these, so the fields that hand them out cannot drift apart on
/// what a link looks like or how long it lasts.
pub(crate) fn signed(state: &AppState, route: &str, download: bool) -> String {
    let query: &[(&str, &str)] = match download {
        true => &[("download", "1")],
        false => &[],
    };
    crate::commands::serve::signed_url::signed_path(
        &state.signer,
        route,
        query,
        leviath_core::duration::now_secs(),
    )
}

/// Where a run is, in its blueprint's own terms.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct CurrentStage {
    /// The stage's name, as the blueprint spells it.
    pub(crate) name: String,
    /// Its position, counting from zero.
    pub(crate) index: i32,
    /// How many stages the blueprint has.
    pub(crate) of: i32,
}

/// One point in a run's history: the whole context window, as it stood.
#[mirror]
#[derive(SimpleObject)]
pub(crate) struct ContextSnapshotPoint {
    /// When the window looked like this.
    pub(crate) at: Timestamp,
    /// The stage the run was in. Empty before the first stage is entered.
    pub(crate) stage: String,
    /// The window itself. Region contents are their own field, so asking for the
    /// shape of a hundred windows does not read a hundred windows' text.
    ///
    /// Its `revision` is this point's stable name: pass it to `contextSnapshot`
    /// to come back to exactly this content, whatever the run does next.
    pub(crate) window: ContextWindow,
}

impl Paged for ContextSnapshotPoint {
    const NAME: &'static str = "ContextSnapshotPoint";
}

/// One replayed point as the object a client reads.
pub(crate) fn snapshot_point(point: leviath_core::run_archive::RunPoint) -> ContextSnapshotPoint {
    ContextSnapshotPoint {
        at: Timestamp(point.at),
        stage: point.meta.current_stage.clone(),
        window: ContextWindow {
            snapshot: std::sync::Arc::new(point.context),
        },
    }
}

/// One page of a run's context history, where nothing is filtered.
///
/// Every point is on the listing, so which of them this page holds is
/// arithmetic over their own positions and only those windows are read. A
/// window is the largest thing this API materializes, and a mature run's
/// journal holds hundreds; reading them all to hand back fifty is the
/// difference between a page and the whole file.
///
/// The positions, the cursor and the direction are exactly
/// [`position_page`](crate::commands::serve::graphql::connection::position_page)'s,
/// so a cursor means the same thing whichever of the two answered the page
/// before it.
pub(crate) async fn unfiltered_history(
    run_id: &str,
    query: PositionQuery<'_>,
) -> Result<PositionPage<ContextSnapshotPoint>, ServeError> {
    let PositionQuery {
        digest,
        after,
        descending,
        limit,
    } = query;
    let counting = run_id.to_string();
    let total = blocking(move || history::point_count(&counting))
        .await
        .unwrap_or_default();
    let mut ordered: Vec<usize> = (0..total).collect();
    if descending {
        ordered.reverse();
    }
    let start = match after {
        None => 0,
        Some(raw) => {
            let boundary = cursor::decode_position(raw, digest, descending)
                .map_err(|e| ServeError::BadRequest(e.message()))?;
            ordered
                .iter()
                .position(|position| match descending {
                    true => *position < boundary,
                    false => *position > boundary,
                })
                .unwrap_or(ordered.len())
        }
    };
    let mut wanted = ordered.split_off(start);
    let has_more = wanted.len() > limit;
    wanted.truncate(limit);
    let cursor = has_more
        .then(|| wanted.last())
        .flatten()
        .map(|position| Cursor(cursor::encode_position(digest, *position, descending)));
    let reading = run_id.to_string();
    let asked = wanted;
    let mut read = blocking(move || history::windows_at(&reading, &asked)).await;
    if descending {
        read.reverse();
    }
    Ok(PositionPage {
        items: read
            .into_iter()
            .map(|(_, point)| snapshot_point(point))
            .collect(),
        cursor,
        total,
    })
}
