//! What `orderBy` compiles to.
//!
//! A listing's order decides three things at once, and they have to agree: how
//! two items compare, what the cursor records so a walk cannot change order
//! halfway through, and what key a cursor carries. [`Order`] is all three,
//! built once per request from the terms the client asked for.
//!
//! **One key encodes exactly as it always did.** A single-term order records
//! the field's wire name as the cursor's `sort` and the direction as its
//! `order`, which is byte for byte what `GET /api/runs` mints - so an
//! unfiltered cursor from either surface resumes on the other. Only a
//! multi-key order needs more, and it says so in its own shape: a `sort` of
//! `started_at:desc,title:asc` and a [`CursorKey::Tuple`] key.

use async_graphql::Enum;

use crate::commands::serve::core::error::ServeError;
use crate::commands::serve::cursor::{self, Cursor, CursorKey};

/// Which way a listing runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum OrderDirection {
    /// Smallest first: oldest, or A to Z.
    Asc,
    /// Largest first: newest, or Z to A. What a console wants of anything
    /// timestamped, which is why it is the default on every order input.
    Desc,
}

impl OrderDirection {
    /// Whether this direction walks from the largest value down.
    pub(crate) fn descending(self) -> bool {
        match self {
            Self::Asc => false,
            Self::Desc => true,
        }
    }

    /// The word a cursor records for this direction.
    pub(crate) fn wire(self) -> &'static str {
        cursor::order_name(self.descending())
    }
}

/// A field a listing may be ordered by.
///
/// The wire name is the method the field is read from, which is also what
/// `SortKey::as_str` spells for REST - the two agreeing is what keeps a cursor
/// interchangeable between the surfaces.
pub(crate) trait OrderField: Copy + Eq {
    /// The name a cursor records for this field.
    fn wire(self) -> &'static str;
}

/// Something a listing can order by one of its own fields.
///
/// The context is a type parameter rather than a fixed type so the trait says
/// nothing about what a key may consult: a run's key needs the clock and the
/// parent map, a blueprint's needs nothing at all.
pub(crate) trait Orderable<Cx: ?Sized> {
    /// The fields this type may be ordered by.
    type Field: OrderField;

    /// This item's value for one order field.
    ///
    /// Only in-memory, time-stable fields belong here: an age moves with the
    /// clock, so a walk ordered by one would resume somewhere else.
    fn key(&self, field: Self::Field, cx: &Cx) -> CursorKey;
}

/// One sort key and the direction it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Term<F> {
    /// Which field.
    pub(crate) field: F,
    /// Which way.
    pub(crate) direction: OrderDirection,
}

/// Where one item sits in a walk: its sort key, and the id that breaks a tie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Position {
    /// The ordering value, or the tuple of them for a multi-key order.
    pub(crate) key: CursorKey,
    /// The item's own id, which is what makes the order total.
    pub(crate) id: String,
}

/// A compiled `orderBy`: how two items compare, and what a cursor records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Order<F> {
    /// The terms, in priority order.
    terms: Vec<Term<F>>,
    /// The cursor's `sort`.
    sort: String,
    /// The cursor's `order`.
    order: String,
    /// One direction per term, for the keyset comparison.
    descending: Vec<bool>,
}

impl<F: OrderField> Order<F> {
    /// Compile a list of terms.
    ///
    /// A single term encodes as the bare field name and direction, which is the
    /// shape REST already mints. Two or more spell both out per term, because a
    /// cursor has one `sort` slot and the directions are part of what the walk
    /// promised.
    pub(crate) fn new(terms: Vec<Term<F>>) -> Self {
        let sort = match terms.as_slice() {
            [only] => only.field.wire().to_string(),
            many => many
                .iter()
                .map(|term| format!("{}:{}", term.field.wire(), term.direction.wire()))
                .collect::<Vec<_>>()
                .join(","),
        };
        let order = terms
            .first()
            .map(|term| term.direction.wire())
            .unwrap_or_else(|| cursor::order_name(false))
            .to_string();
        let descending = terms
            .iter()
            .map(|term| term.direction.descending())
            .collect();
        Self {
            terms,
            sort,
            order,
            descending,
        }
    }

    /// The `sort` a cursor from this order records, and is checked against.
    pub(crate) fn sort(&self) -> &str {
        &self.sort
    }

    /// The `order` a cursor from this order records: the primary direction.
    pub(crate) fn order(&self) -> &str {
        &self.order
    }

    /// One direction per key component, for the keyset comparison.
    pub(crate) fn descending(&self) -> &[bool] {
        &self.descending
    }

    /// Where `item` sits under this order.
    ///
    /// A single term gives the bare key, so the cursor is byte-identical to
    /// REST's. No terms at all leaves the id to do the whole job, which is
    /// still a total order.
    pub(crate) fn position<Cx, T>(&self, item: &T, id: String, cx: &Cx) -> Position
    where
        Cx: ?Sized,
        T: Orderable<Cx, Field = F>,
    {
        let key = match self.terms.as_slice() {
            [] => CursorKey::Null,
            [only] => item.key(only.field, cx),
            many => CursorKey::Tuple(many.iter().map(|term| item.key(term.field, cx)).collect()),
        };
        Position { key, id }
    }

    /// Mint the cursor that resumes just past `position`.
    pub(crate) fn encode(&self, digest: &str, position: &Position) -> String {
        cursor::encode(
            self.sort(),
            self.order(),
            digest,
            position.key.clone(),
            &position.id,
        )
    }

    /// Read a cursor back, refusing one minted for another order or filter.
    pub(crate) fn decode(&self, raw: &str, digest: &str) -> Result<Cursor, ServeError> {
        cursor::decode(raw, self.sort(), self.order(), digest)
            .map_err(|e| ServeError::BadRequest(e.message()))
    }
}

#[cfg(test)]
#[path = "order_tests.rs"]
mod tests;
