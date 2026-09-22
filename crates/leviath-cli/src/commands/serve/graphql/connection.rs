//! Paged listings.
//!
//! The REST routes hand out a `Page` envelope with a `next_cursor`, and a
//! GraphQL listing says the same thing. Both are the same keyset walk
//! underneath, and the cursor tokens are interchangeable between the two
//! surfaces.
//!
//! [`Connection`] is the one shape every listing settles on: `results`,
//! `cursor`, `total` and nothing else. Its type name comes from what it holds,
//! so one generic produces `RunConnection`, `BlueprintConnection` and the rest
//! from one definition, and a listing that genuinely has something more to say
//! flattens it in beside the three.
//!
//! `total` is a resolver rather than a struct field, and it is flattened in
//! from [`Total`] for exactly that reason. Counting a listing whose filter
//! reads files costs a pass over the store; a client that only wanted a page
//! must not pay for it because the count happened to sit in the same struct.
//! Selecting `total` runs the count. Not selecting it runs nothing.

use std::borrow::Cow;

use async_graphql::{Object, ObjectType, OutputType, SimpleObject, TypeName};
use futures_util::future::BoxFuture;
use tokio::sync::Mutex;

use super::filter::{Filterable, MatchCx};
use super::scalars::Cursor;
use crate::commands::serve::core::error::ServeError;
use crate::commands::serve::cursor;

/// A type that can be paged, and the name its connection takes.
///
/// The name is spelled here rather than read from `OutputType::type_name()`
/// because a connection's name has to be known before the item type is
/// registered, and because a listing of a type is named after the type even
/// where the two are not spelled alike.
pub(crate) trait Paged {
    /// The GraphQL name of the item, which `Connection` suffixes.
    const NAME: &'static str;
}

/// A listing with nothing to say beyond the three core fields.
///
/// It contributes no fields, so it leaves no trace in the schema: an object
/// nothing refers to is dropped when the schema is built.
#[derive(Debug, SimpleObject)]
#[graphql(fake)]
pub(crate) struct NoExtras;

/// How a connection counts the listing its page is one slice of.
///
/// A future rather than a number, because building it is what a page can
/// afford and running it is what it cannot. Nothing here runs until
/// [`Connection::total`] is selected.
type Counter = BoxFuture<'static, usize>;

/// Either the count, or what it would take to get it.
enum Counting {
    /// Already known: a listing that counted itself on the way past.
    Counted(i32),
    /// Not counted yet.
    Uncounted(Counter),
}

/// The `total` field, and the count behind it.
///
/// Its own object, flattened into the connection, because `total` is the one
/// field that must not be computed unless it was asked for: a resolver can
/// decline to run, a struct field cannot. It contributes exactly the one field
/// and is dropped from the schema as an unreferenced type.
pub(crate) struct Total {
    /// Guarded because the field is resolved through a shared reference, and
    /// a query may name it more than once.
    state: Mutex<Counting>,
}

/// Carries a listing's `total`.
#[Object]
impl Total {
    /// How many items match, across every page.
    ///
    /// Counted only when it is selected. For a filter answerable from memory
    /// that is free; for one that reads files it is a pass over the store, so
    /// ask for it on the first page rather than on every one.
    async fn total(&self) -> i32 {
        self.count().await
    }
}

impl Total {
    /// A count the listing already holds.
    ///
    /// For anything bounded by a definition - a run's journal, a blueprint's
    /// stages - the length is the count, and there is nothing to defer.
    pub(crate) fn known(total: usize) -> Self {
        Self {
            state: Mutex::new(Counting::Counted(clamp(total))),
        }
    }

    /// A count that runs only if it is asked for.
    pub(crate) fn lazy<F>(counter: F) -> Self
    where
        F: std::future::Future<Output = usize> + Send + 'static,
    {
        Self {
            state: Mutex::new(Counting::Uncounted(Box::pin(counter))),
        }
    }

    /// The count, running it the first time and remembering it after.
    async fn count(&self) -> i32 {
        let mut state = self.state.lock().await;
        // Taken out so the future can be driven, and put back as a number so a
        // second mention of the field in one query costs nothing.
        let total = match std::mem::replace(&mut *state, Counting::Counted(0)) {
            Counting::Counted(known) => known,
            Counting::Uncounted(counter) => clamp(counter.await),
        };
        *state = Counting::Counted(total);
        total
    }
}

/// Fit a count into the `Int` a client reads it as.
///
/// A run store large enough to overflow this would have other problems, and
/// saturating says "at least this many" where wrapping would say something
/// false.
fn clamp(total: usize) -> i32 {
    i32::try_from(total).unwrap_or(i32::MAX)
}

/// One page of a listing: the items, where to resume, and how many there are
/// altogether.
#[derive(SimpleObject)]
#[graphql(name_type)]
pub(crate) struct Connection<T: Paged + OutputType, X: ObjectType = NoExtras> {
    /// The items on this page, at most `first` of them.
    results: Vec<T>,
    /// Pass as `after` to read the next page. Null when this is the last one.
    cursor: Option<Cursor>,
    /// The count of the whole listing, deferred until it is selected.
    #[graphql(flatten)]
    total: Total,
    /// Whatever this listing adds beyond the three fields above.
    #[graphql(flatten)]
    extras: X,
}

impl<T: Paged + OutputType, X: ObjectType> Connection<T, X> {
    /// Build a page.
    pub(crate) fn new(results: Vec<T>, cursor: Option<Cursor>, total: Total, extras: X) -> Self {
        Self {
            results,
            cursor,
            total,
            extras,
        }
    }
}

impl<T: Paged + OutputType> Connection<T, NoExtras> {
    /// Build a page of a listing that has nothing extra to say.
    pub(crate) fn plain(results: Vec<T>, cursor: Option<Cursor>, total: Total) -> Self {
        Self::new(results, cursor, total, NoExtras)
    }

    /// Give a page something extra to say, without rebuilding it.
    ///
    /// The three core fields are the same walk's answer whatever a listing adds
    /// beside them, so the walk is written once and hands back a plain page
    /// that the listing then flattens its own fields into.
    pub(crate) fn with_extras<X: ObjectType>(self, extras: X) -> Connection<T, X> {
        Connection {
            results: self.results,
            cursor: self.cursor,
            total: self.total,
            extras,
        }
    }
}

impl<T: Paged + OutputType, X: ObjectType> TypeName for Connection<T, X> {
    fn type_name() -> Cow<'static, str> {
        Cow::Owned(format!("{}Connection", T::NAME))
    }
}

/// One page of a listing whose whole record set the one read that answers it
/// already holds: a run's own executions, attempts, interactions, context
/// changes and history points, and a directory's own file listing.
///
/// Unlike the run store, there is no second file to avoid opening by scanning
/// lazily - the read already happened - so a file-backed filter is confirmed
/// across the whole set once, up front, rather than deferred page by page.
/// What still matters is the position: each item's own place in what was
/// read, ascending, which is what [`crate::commands::serve::cursor::encode_position`]
/// and [`crate::commands::serve::cursor::decode_position`] key a cursor on.
pub(crate) struct PositionPage<T> {
    /// The items on this page, in walk order.
    pub(crate) items: Vec<T>,
    /// Where the next page starts. Absent when this page reached the end.
    pub(crate) cursor: Option<Cursor>,
    /// How many items matched altogether, page and all.
    pub(crate) total: usize,
}

/// The part of a [`position_page`] call that names the page itself, bundled so
/// the function stays under the arity a reader can hold in mind: what to walk
/// and how to filter it are one kind of argument, where to cut the page is
/// another.
pub(crate) struct PositionQuery<'a> {
    /// The digest this listing's cursor is bound to.
    pub(crate) digest: &'a str,
    /// The previous page's cursor, if this is not the first.
    pub(crate) after: Option<&'a str>,
    /// Whether the walk runs newest (or largest) first.
    pub(crate) descending: bool,
    /// How many items this page holds at most.
    pub(crate) limit: usize,
}

/// Filter, order and page a listing already fully in memory.
///
/// The position [`encode_position`](cursor::encode_position) records is each
/// item's own place in `items` as they were recorded, ascending, whichever way
/// round the walk runs: not a field of the item, because more than one item can
/// share a journal position - every call one batch dispatched together does -
/// and a cursor has to name a place the walk cannot repeat or skip, which only
/// the list's own order guarantees.
///
/// Counted from the start rather than from the walk's own end, because the
/// journal behind these listings grows: a run appending an execution moves
/// every position counted from the end, and a descending page two would then
/// hand back rows page one already showed. A descending walk reads the same
/// ascending positions in reverse and resumes below the boundary rather than
/// above it. The direction is still part of the cursor's identity, so a cursor
/// minted walking one way is refused walking the other.
pub(crate) async fn position_page<T>(
    items: Vec<T>,
    filter: &T::Filter,
    cx: &MatchCx<'_>,
    query: PositionQuery<'_>,
) -> Result<PositionPage<T>, ServeError>
where
    T: Filterable,
{
    let PositionQuery {
        digest,
        after,
        descending,
        limit,
    } = query;
    let mut matched: Vec<(usize, T)> = Vec::with_capacity(items.len());
    for (position, item) in items.into_iter().enumerate() {
        // The cheap answer decides on its own where it can, and only an
        // undecided one reads; `confirm` is what settles both.
        if item.confirm(filter, cx).await {
            matched.push((position, item));
        }
    }
    if descending {
        matched.reverse();
    }
    let total = matched.len();
    let start = match after {
        None => 0,
        Some(raw) => {
            let boundary = cursor::decode_position(raw, digest, descending)
                .map_err(|e| ServeError::BadRequest(e.message()))?;
            matched
                .iter()
                .position(|(position, _)| match descending {
                    true => *position < boundary,
                    false => *position > boundary,
                })
                .unwrap_or(matched.len())
        }
    };
    let mut page: Vec<(usize, T)> = matched.split_off(start.min(matched.len()));
    let has_more = page.len() > limit;
    page.truncate(limit);
    let cursor = has_more
        .then(|| page.last())
        .flatten()
        .map(|(position, _)| Cursor(cursor::encode_position(digest, *position, descending)));
    Ok(PositionPage {
        items: page.into_iter().map(|(_, item)| item).collect(),
        cursor,
        total,
    })
}

/// Declare the `orderBy` shape for a listing paged by [`position_page`]: one
/// sort key, its own place in what was read.
///
/// Five of this schema's listings are shaped this way - a run's executions,
/// attempts, interactions, context changes and history points - because the
/// one read that answers each of them already puts everything in that order,
/// and there is nothing else in memory worth sorting by. `#[filter(orderable)]`
/// does not fit here: the mirror marks a *resolver*, and these listings'
/// position is not one, whether because it is a plain field a client is not
/// otherwise offered (an attempt's or an interaction's place among the run's
/// own) or because the listing merges two sources with no field in common
/// (`InteractionOutput`). Naming the one field this offers only ever changes
/// the direction; `orderBy` omitted gives that order ascending, the order the
/// run recorded it in.
macro_rules! position_order {
    ($order:ident, $field:ident, $variant:ident, $doc:literal, $field_doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, ::async_graphql::Enum)]
        pub(crate) enum $field {
            #[doc = $field_doc]
            $variant,
        }

        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, ::async_graphql::InputObject)]
        pub(crate) struct $order {
            /// Which field. There is only the one.
            pub(crate) field: $field,
            /// Which way it runs.
            #[graphql(default_with = "super::super::paging::order::OrderDirection::Desc")]
            pub(crate) direction: super::super::paging::order::OrderDirection,
        }
    };
}

pub(crate) use position_order;

#[cfg(test)]
#[path = "connection_tests.rs"]
mod tests;
