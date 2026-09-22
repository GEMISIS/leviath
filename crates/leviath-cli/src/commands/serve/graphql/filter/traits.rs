//! The contract between the `#[mirror]` macro and this module.
//!
//! Everything the macro writes is an impl of one of these traits, and every
//! one of those impls is data and delegation: a list of fields, and a call
//! into `engine.rs`. No decision the filter system makes is written per type,
//! so there is one copy of each rule to read and one copy to measure.

use std::future::Future;
use std::pin::Pin;

use async_graphql::InputType;

use super::{Acc, CursorKey, MatchCx, Tri};

/// A future the runtime hands back from a trait method.
///
/// Filters recurse into themselves, and a recursive `async fn` has to be boxed
/// somewhere. It is boxed here, once, rather than at every nesting level.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A value some GraphQL input can be tested against.
///
/// The associated filter is what the client writes. Scalars map onto the
/// shared wire filters in `scalars.rs`; a mirrored output type maps onto the
/// input object the macro wrote for it.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no filter, so a field of this type cannot be mirrored",
    label = "no `Filterable` impl for this type",
    note = "put `#[mirror]` on the type, or `#[filter(skip)]` on the field to leave it out of the mirror"
)]
pub(crate) trait Filterable: Sync {
    /// The GraphQL input this value is filtered with.
    type Filter: InputType;

    /// What this filter says about this value without reading anything.
    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri;

    /// The full answer, with whatever reads it takes.
    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool>;
}

/// A value a list of which can be filtered.
///
/// `Vec<T>` and `[T]` are filtered through this, so a list field needs its item
/// type to carry a quantifier input. For a mirrored type that is what
/// `#[mirror(list)]` writes.
#[diagnostic::on_unimplemented(
    message = "a list of `{Self}` cannot be filtered: `{Self}` has no list input",
    label = "no `ListItem` impl for this type",
    note = "write `#[mirror(list)]` on `{Self}` so it gets its `some` / `every` / `none` input"
)]
pub(crate) trait ListItem: Filterable + Sized {
    /// The GraphQL input a list of these values is filtered with.
    type ListFilter: InputType;

    /// What this quantifier says about these items without reading anything.
    fn list_test(items: &[Self], filter: &Self::ListFilter, cx: &MatchCx<'_>) -> Tri;

    /// The full answer for these items, with whatever reads it takes.
    fn list_confirm<'a>(
        items: &'a [Self],
        filter: &'a Self::ListFilter,
        cx: &'a MatchCx<'a>,
    ) -> BoxFuture<'a, bool>;
}

/// A filter input that carries `isNull`.
///
/// Every filter in the schema carries it, which is what lets the one rule for
/// an absent value live in `values::option_test` instead of in each filter.
pub(crate) trait Nullable {
    /// What the client asked about the value being absent.
    fn is_null(&self) -> Option<bool>;
}

/// The generated input object that mirrors one output type.
///
/// `parts` hands over the combinators, `cheap` folds the in-memory fields, and
/// `io` re-answers every field with reads allowed. Splitting it this way is
/// what makes the cheap pass a pure function of memory.
pub(crate) trait Mirror: Sized + Send + Sync + Nullable {
    /// The output type this input mirrors.
    type Target: Sync;

    /// The `and` / `or` / `not` / `isNull` this filter carries.
    fn parts(&self) -> Parts<'_, Self>;

    /// Fold every field that can be answered from memory.
    fn cheap(&self, target: &Self::Target, cx: &MatchCx<'_>, acc: &mut Acc);

    /// Answer every field of this one level, reading whatever that takes.
    ///
    /// Fields that cost nothing are answered again here rather than carried
    /// over from the cheap pass: they are cheap, and re-asking is what keeps
    /// the two phases one definition apart instead of two.
    fn io<'a>(&'a self, target: &'a Self::Target, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool>;
}

/// The combinators every mirror carries, borrowed from one filter value.
#[derive(Debug)]
pub(crate) struct Parts<'a, M> {
    /// Every filter here has to hold.
    pub(crate) and: Option<&'a [M]>,
    /// At least one filter here has to hold.
    pub(crate) or: Option<&'a [M]>,
    /// This filter must not hold.
    pub(crate) not: Option<&'a M>,
    /// What the client asked about the value being absent.
    pub(crate) is_null: Option<bool>,
}

impl<'a, M> Parts<'a, M> {
    /// Gather the four combinators a generated `parts` borrowed.
    pub(crate) fn new(
        and: Option<&'a [M]>,
        or: Option<&'a [M]>,
        not: Option<&'a M>,
        is_null: Option<bool>,
    ) -> Self {
        Self {
            and,
            or,
            not,
            is_null,
        }
    }
}

/// The generated input object that quantifies over a list.
pub(crate) trait Quantified: Nullable + Sync {
    /// The item type the quantifiers are about.
    type Item: Filterable;

    /// The `some` / `every` / `none` this filter carries.
    fn quantifiers(&self) -> Quantifiers<'_, <Self::Item as Filterable>::Filter>;
}

/// The three quantifiers, borrowed from one list filter value.
#[derive(Debug)]
pub(crate) struct Quantifiers<'a, F> {
    /// At least one item matches.
    pub(crate) some: Option<&'a F>,
    /// Every item matches.
    pub(crate) every: Option<&'a F>,
    /// No item matches.
    pub(crate) none: Option<&'a F>,
}

impl<'a, F> Quantifiers<'a, F> {
    /// Gather the three quantifiers a generated `quantifiers` borrowed.
    pub(crate) fn new(some: Option<&'a F>, every: Option<&'a F>, none: Option<&'a F>) -> Self {
        Self { some, every, none }
    }
}

/// The generated input object that compares an enum.
pub(crate) trait EnumParts: Nullable {
    /// The enum being compared.
    type Value: Copy + PartialEq;

    /// The comparisons this filter carries.
    fn choices(&self) -> Choices<'_, Self::Value>;
}

/// An enum filter's comparisons, borrowed from one filter value.
#[derive(Debug)]
pub(crate) struct Choices<'a, T> {
    /// Exactly this value.
    pub(crate) eq: Option<T>,
    /// Anything but this value.
    pub(crate) ne: Option<T>,
    /// One of these values.
    pub(crate) within: Option<&'a [T]>,
    /// None of these values.
    pub(crate) not_in: Option<&'a [T]>,
    /// What the client asked about the value being absent.
    pub(crate) is_null: Option<bool>,
}

impl<'a, T> Choices<'a, T> {
    /// Gather the comparisons a generated `choices` borrowed.
    pub(crate) fn new(
        eq: Option<T>,
        ne: Option<T>,
        within: Option<&'a [T]>,
        not_in: Option<&'a [T]>,
        is_null: Option<bool>,
    ) -> Self {
        Self {
            eq,
            ne,
            within,
            not_in,
            is_null,
        }
    }
}

// ── The ordering half of the contract ──────────────────────────────────────
//
// `OrderField`, `Orderable` and `CursorKey` are `graphql/paging`'s, and
// `filter/mod.rs` re-exports them so the macro's one `rt` path finds them.
// What belongs here is the step between them: the value a mirrored field
// answers with, as the key a cursor records.

/// A value that can be a sort key.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be sorted on, so this field cannot be `#[filter(orderable)]`",
    label = "no `Sortable` impl for this type",
    note = "only in-memory, time-stable fields are orderable: a text, a number, a timestamp, or an optional one of those"
)]
pub(crate) trait Sortable {
    /// This value as the cursor records it.
    fn cursor_key(&self) -> CursorKey;
}

/// The cursor key one orderable field's value sorts by.
///
/// The impls are in `values.rs`, beside the `Filterable` impls for the same
/// types, so what a field can do sits in one place per type.
pub(crate) fn sort_key<V: Sortable + ?Sized>(value: &V) -> CursorKey {
    value.cursor_key()
}

#[cfg(test)]
#[path = "traits_tests.rs"]
mod tests;
