//! The adapter between a mirror and the walk.
//!
//! A listing holds three things: the filter the client wrote, the order it
//! asked for, and the clock the whole request shares. The walk asks for a
//! verdict, a confirmation and a position. Written once here, so a new listing
//! is a resolver and not a fourth copy of the same four methods.

use futures_util::future::BoxFuture;

use super::super::paging::order::{Order, Position};
use super::super::paging::walk::{Sift, Verdict};
use super::{Filterable, MatchCx, OrderField, Orderable, Tri};

/// What the cheap pass said, as the walk reads it.
///
/// The three answers line up one for one, and this is the only place they are
/// translated: `Io` means the walk should read, and nothing else does.
pub(crate) fn verdict(answer: Tri) -> Verdict {
    match answer {
        Tri::Yes => Verdict::Keep,
        Tri::No => Verdict::Drop,
        Tri::Io => Verdict::NeedsIo,
    }
}

/// One listing's filter and order, as the walk consults them.
pub(crate) struct Sifted<T: Filterable, F> {
    /// What the client asked for, as the mirror of the type being listed.
    filter: T::Filter,
    /// The clock and relations every test in this request shares.
    cx: MatchCx<'static>,
    /// The compiled `orderBy`.
    order: Order<F>,
    /// The item's own id, which is what makes the order total.
    id: fn(&T) -> String,
}

impl<T: Filterable, F> Sifted<T, F> {
    /// Gather what one listing walks with.
    pub(crate) fn new(
        filter: T::Filter,
        cx: MatchCx<'static>,
        order: Order<F>,
        id: fn(&T) -> String,
    ) -> Self {
        Self {
            filter,
            cx,
            order,
            id,
        }
    }

    /// The order this listing runs in, for the caller that mints its cursor.
    pub(crate) fn order(&self) -> &Order<F> {
        &self.order
    }
}

impl<T, F> Sift for Sifted<T, F>
where
    T: Filterable + Orderable<MatchCx<'static>, Field = F> + Send + Sync,
    T::Filter: Send + Sync,
    F: OrderField + Send + Sync,
{
    type Item = T;

    fn test(&self, item: &Self::Item) -> Verdict {
        verdict(item.test(&self.filter, &self.cx))
    }

    fn confirm<'a>(&'a self, item: &'a Self::Item) -> BoxFuture<'a, bool> {
        item.confirm(&self.filter, &self.cx)
    }

    fn position(&self, item: &Self::Item) -> Position {
        self.order.position(item, (self.id)(item), &self.cx)
    }

    fn descending(&self) -> &[bool] {
        self.order.descending()
    }
}

#[cfg(test)]
#[path = "sift_tests.rs"]
mod tests;
