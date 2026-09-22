//! The shared wire filters every mirrored field bottoms out in.
//!
//! One input object per scalar, reused wherever a field of that scalar can be
//! filtered. A client that has learnt `StringFilter` on a run's title knows it
//! on a blueprint's description, and a field that becomes filterable adds a
//! line rather than a type.
//!
//! Every field set in one filter object has to hold. Leaving a field out is
//! not a wildcard match on that field, it is the absence of that comparison.
//! `isNull` is the one field that is about the value's presence rather than
//! its contents, and it is on every filter in the schema so that "has no
//! title" is a thing a client can say directly.

use std::cmp::Ordering;

use async_graphql::{ID, InputObject};

use super::super::scalars::{BigInt, Decimal, Json, Timestamp};
use super::Nullable;

/// How one bound compares to a value.
///
/// The bound is the wire type the client wrote and the value is the type the
/// field is read as, which is not always the same: a `BigInt` bound compares
/// against a plain `i64` a resolver returned, so a widening lives here once
/// rather than at every call.
pub(crate) trait Bound<V: ?Sized> {
    /// Where `value` sits relative to this bound, or nothing when the two do
    /// not compare at all.
    fn compare(&self, value: &V) -> Option<Ordering>;
}

/// Every comparison an ordered filter can carry, borrowed from one value.
#[derive(Debug)]
pub(crate) struct Ordered<'a, B> {
    /// Equal to this.
    eq: Option<&'a B>,
    /// Not equal to this.
    ne: Option<&'a B>,
    /// One of these.
    within: Option<&'a [B]>,
    /// None of these.
    not_in: Option<&'a [B]>,
    /// Strictly below this.
    lt: Option<&'a B>,
    /// At or below this.
    lte: Option<&'a B>,
    /// Strictly above this.
    gt: Option<&'a B>,
    /// At or above this.
    gte: Option<&'a B>,
    /// What the client asked about the value being absent.
    is_null: Option<bool>,
}

impl<'a, B> Ordered<'a, B> {
    /// The four comparisons every filter has, with no range bounds.
    pub(crate) fn exact(
        eq: Option<&'a B>,
        ne: Option<&'a B>,
        within: Option<&'a [B]>,
        not_in: Option<&'a [B]>,
        is_null: Option<bool>,
    ) -> Self {
        Self {
            eq,
            ne,
            within,
            not_in,
            lt: None,
            lte: None,
            gt: None,
            gte: None,
            is_null,
        }
    }

    /// The same comparisons with the four range bounds added.
    pub(crate) fn range(
        self,
        lt: Option<&'a B>,
        lte: Option<&'a B>,
        gt: Option<&'a B>,
        gte: Option<&'a B>,
    ) -> Self {
        Self {
            lt,
            lte,
            gt,
            gte,
            ..self
        }
    }
}

/// Whether a value satisfies the four comparisons every filter carries.
///
/// A value is here, so `isNull: true` refuses it whatever else is set.
pub(crate) fn exact_test<V, B>(value: &V, filter: &Ordered<'_, B>) -> bool
where
    V: ?Sized,
    B: Bound<V>,
{
    let same = |bound: &B| bound.compare(value) == Some(Ordering::Equal);
    filter.is_null != Some(true)
        && filter.eq.is_none_or(&same)
        && filter.ne.is_none_or(|bound| !same(bound))
        && filter.within.is_none_or(|set| set.iter().any(same))
        && filter.not_in.is_none_or(|set| !set.iter().any(same))
}

/// Whether a value satisfies every comparison an ordered filter carries.
///
/// The four range bounds are only on the filters that offer them, so the
/// filters that compare for equality alone go through [`exact_test`] and have
/// no comparison in them that nothing can reach.
pub(crate) fn ordered_test<V, B>(value: &V, filter: &Ordered<'_, B>) -> bool
where
    V: ?Sized,
    B: Bound<V>,
{
    exact_test(value, filter)
        && filter
            .lt
            .is_none_or(|bound| bound.compare(value) == Some(Ordering::Less))
        && filter
            .lte
            .is_none_or(|bound| bound.compare(value) != Some(Ordering::Greater))
        && filter
            .gt
            .is_none_or(|bound| bound.compare(value) == Some(Ordering::Greater))
        && filter
            .gte
            .is_none_or(|bound| bound.compare(value) != Some(Ordering::Less))
}

/// Text comparisons on one field.
///
/// `eq`, `ne`, `in` and `notIn` compare the whole string exactly, case
/// included. `contains`, `startsWith` and `endsWith` ignore ASCII case, as the
/// run search does. Every field set here has to hold.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct StringFilter {
    /// Exactly this string.
    pub(crate) eq: Option<String>,
    /// Anything but this string.
    pub(crate) ne: Option<String>,
    /// Exactly one of these strings.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<String>>,
    /// None of these strings.
    pub(crate) not_in: Option<Vec<String>>,
    /// Holds this substring, ignoring ASCII case.
    pub(crate) contains: Option<String>,
    /// Begins with this, ignoring ASCII case.
    pub(crate) starts_with: Option<String>,
    /// Ends with this, ignoring ASCII case.
    pub(crate) ends_with: Option<String>,
    /// Match where there is no string at all.
    pub(crate) is_null: Option<bool>,
}

/// Whether a string satisfies every comparison a text filter carries.
pub(crate) fn text_test(value: &str, filter: &StringFilter) -> bool {
    let folded = value.to_ascii_lowercase();
    filter.is_null != Some(true)
        && filter.eq.as_deref().is_none_or(|bound| value == bound)
        && filter.ne.as_deref().is_none_or(|bound| value != bound)
        && filter
            .within
            .as_ref()
            .is_none_or(|set| set.iter().any(|bound| bound == value))
        && filter
            .not_in
            .as_ref()
            .is_none_or(|set| !set.iter().any(|bound| bound == value))
        && filter
            .contains
            .as_deref()
            .is_none_or(|bound| folded.contains(&bound.to_ascii_lowercase()))
        && filter
            .starts_with
            .as_deref()
            .is_none_or(|bound| folded.starts_with(&bound.to_ascii_lowercase()))
        && filter
            .ends_with
            .as_deref()
            .is_none_or(|bound| folded.ends_with(&bound.to_ascii_lowercase()))
}

/// Exact and set tests on an id.
///
/// An id is compared as the opaque token it is: whole, and case-sensitively.
/// There is no `contains` here on purpose, because a client matching part of
/// an id has coupled itself to how the id is built.
// async-graphql PascalCases a Rust name into a GraphQL one, which would turn
// this into `Idfilter`. The scalar is `ID`, so the filter is `IDFilter`.
#[derive(Clone, Debug, Default, InputObject)]
#[graphql(name = "IDFilter")]
pub(crate) struct IDFilter {
    /// Exactly this id.
    pub(crate) eq: Option<ID>,
    /// Anything but this id.
    pub(crate) ne: Option<ID>,
    /// Exactly one of these ids.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<ID>>,
    /// None of these ids.
    pub(crate) not_in: Option<Vec<ID>>,
    /// Match where there is no id at all.
    pub(crate) is_null: Option<bool>,
}

impl IDFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, ID> {
        Ordered::exact(
            self.eq.as_ref(),
            self.ne.as_ref(),
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
    }
}

impl Bound<ID> for ID {
    fn compare(&self, value: &ID) -> Option<Ordering> {
        value.as_str().partial_cmp(self.as_str())
    }
}

/// Comparisons on one whole-number field.
///
/// The range fields are what "older than", "ran longer than" and every bound
/// between are asked with. Every field set here has to hold, so `gte` and `lt`
/// together are a half-open range.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct IntFilter {
    /// Exactly this number.
    pub(crate) eq: Option<i32>,
    /// Anything but this number.
    pub(crate) ne: Option<i32>,
    /// Exactly one of these numbers.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<i32>>,
    /// None of these numbers.
    pub(crate) not_in: Option<Vec<i32>>,
    /// Strictly below this.
    pub(crate) lt: Option<i32>,
    /// At or below this.
    pub(crate) lte: Option<i32>,
    /// Strictly above this.
    pub(crate) gt: Option<i32>,
    /// At or above this.
    pub(crate) gte: Option<i32>,
    /// Match where there is no number at all.
    pub(crate) is_null: Option<bool>,
}

impl IntFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, i32> {
        Ordered::exact(
            self.eq.as_ref(),
            self.ne.as_ref(),
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
        .range(
            self.lt.as_ref(),
            self.lte.as_ref(),
            self.gt.as_ref(),
            self.gte.as_ref(),
        )
    }
}

impl Bound<i32> for i32 {
    fn compare(&self, value: &i32) -> Option<Ordering> {
        value.partial_cmp(self)
    }
}

impl Bound<i64> for i32 {
    fn compare(&self, value: &i64) -> Option<Ordering> {
        value.partial_cmp(&i64::from(*self))
    }
}

/// Comparisons on one 64-bit number, such as a byte count.
///
/// `Int` is 32 bits and a file size is not, so the bounds travel as `BigInt`
/// the way the values do.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct BigIntFilter {
    /// Exactly this number.
    pub(crate) eq: Option<BigInt>,
    /// Anything but this number.
    pub(crate) ne: Option<BigInt>,
    /// Exactly one of these numbers.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<BigInt>>,
    /// None of these numbers.
    pub(crate) not_in: Option<Vec<BigInt>>,
    /// Strictly below this.
    pub(crate) lt: Option<BigInt>,
    /// At or below this.
    pub(crate) lte: Option<BigInt>,
    /// Strictly above this.
    pub(crate) gt: Option<BigInt>,
    /// At or above this.
    pub(crate) gte: Option<BigInt>,
    /// Match where there is no number at all.
    pub(crate) is_null: Option<bool>,
}

impl BigIntFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, BigInt> {
        Ordered::exact(
            self.eq.as_ref(),
            self.ne.as_ref(),
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
        .range(
            self.lt.as_ref(),
            self.lte.as_ref(),
            self.gt.as_ref(),
            self.gte.as_ref(),
        )
    }
}

impl Bound<BigInt> for BigInt {
    fn compare(&self, value: &BigInt) -> Option<Ordering> {
        value.0.partial_cmp(&self.0)
    }
}

impl Bound<i64> for BigInt {
    fn compare(&self, value: &i64) -> Option<Ordering> {
        value.partial_cmp(&self.0)
    }
}

/// Comparisons on one binary floating-point field.
///
/// Money is never one of these: a cost is a `Decimal`, and its filter is
/// below. This is for the measurements that are floating point all the way
/// down, such as a temperature setting.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct FloatFilter {
    /// Exactly this number.
    pub(crate) eq: Option<f64>,
    /// Anything but this number.
    pub(crate) ne: Option<f64>,
    /// Exactly one of these numbers.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<f64>>,
    /// None of these numbers.
    pub(crate) not_in: Option<Vec<f64>>,
    /// Strictly below this.
    pub(crate) lt: Option<f64>,
    /// At or below this.
    pub(crate) lte: Option<f64>,
    /// Strictly above this.
    pub(crate) gt: Option<f64>,
    /// At or above this.
    pub(crate) gte: Option<f64>,
    /// Match where there is no number at all.
    pub(crate) is_null: Option<bool>,
}

impl FloatFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, f64> {
        Ordered::exact(
            self.eq.as_ref(),
            self.ne.as_ref(),
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
        .range(
            self.lt.as_ref(),
            self.lte.as_ref(),
            self.gt.as_ref(),
            self.gte.as_ref(),
        )
    }
}

impl Bound<f64> for f64 {
    fn compare(&self, value: &f64) -> Option<Ordering> {
        value.partial_cmp(self)
    }
}

/// Comparisons on one exact-decimal field, such as spend.
///
/// The bounds travel as decimal strings, the way every `Decimal` in this
/// schema does, so a cost bound a JSON parser would re-round is written out
/// rather than rounded.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct DecimalFilter {
    /// Exactly this amount.
    pub(crate) eq: Option<Decimal>,
    /// Anything but this amount.
    pub(crate) ne: Option<Decimal>,
    /// Exactly one of these amounts.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<Decimal>>,
    /// None of these amounts.
    pub(crate) not_in: Option<Vec<Decimal>>,
    /// Strictly below this.
    pub(crate) lt: Option<Decimal>,
    /// At or below this.
    pub(crate) lte: Option<Decimal>,
    /// Strictly above this.
    pub(crate) gt: Option<Decimal>,
    /// At or above this.
    pub(crate) gte: Option<Decimal>,
    /// Match where there is no amount at all.
    pub(crate) is_null: Option<bool>,
}

impl DecimalFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, Decimal> {
        Ordered::exact(
            self.eq.as_ref(),
            self.ne.as_ref(),
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
        .range(
            self.lt.as_ref(),
            self.lte.as_ref(),
            self.gt.as_ref(),
            self.gte.as_ref(),
        )
    }
}

impl Bound<Decimal> for Decimal {
    fn compare(&self, value: &Decimal) -> Option<Ordering> {
        value.0.partial_cmp(&self.0)
    }
}

/// Comparisons on one timestamp field, in unix epoch seconds.
///
/// "Started before this", "touched since that" and every window between.
/// Every field set here has to hold, so `gte` and `lt` together are a
/// half-open window.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct TimestampFilter {
    /// Exactly this second.
    pub(crate) eq: Option<Timestamp>,
    /// Anything but this second.
    pub(crate) ne: Option<Timestamp>,
    /// Exactly one of these seconds.
    #[graphql(name = "in")]
    pub(crate) within: Option<Vec<Timestamp>>,
    /// None of these seconds.
    pub(crate) not_in: Option<Vec<Timestamp>>,
    /// Strictly before this second.
    pub(crate) lt: Option<Timestamp>,
    /// At or before this second.
    pub(crate) lte: Option<Timestamp>,
    /// Strictly after this second.
    pub(crate) gt: Option<Timestamp>,
    /// At or after this second.
    pub(crate) gte: Option<Timestamp>,
    /// Match where there is no time at all.
    pub(crate) is_null: Option<bool>,
}

impl TimestampFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, Timestamp> {
        Ordered::exact(
            self.eq.as_ref(),
            self.ne.as_ref(),
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
        .range(
            self.lt.as_ref(),
            self.lte.as_ref(),
            self.gt.as_ref(),
            self.gte.as_ref(),
        )
    }
}

impl Bound<Timestamp> for Timestamp {
    fn compare(&self, value: &Timestamp) -> Option<Ordering> {
        value.0.partial_cmp(&self.0)
    }
}

/// Comparisons on one boolean field.
///
/// Two fields rather than one because `ne` reads as itself beside every other
/// filter in the schema. `eq: false` and `ne: true` select the same values.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct BooleanFilter {
    /// Exactly this.
    pub(crate) eq: Option<bool>,
    /// Anything but this.
    pub(crate) ne: Option<bool>,
    /// Match where there is no answer either way.
    pub(crate) is_null: Option<bool>,
}

impl BooleanFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, bool> {
        Ordered::exact(self.eq.as_ref(), self.ne.as_ref(), None, None, self.is_null)
    }
}

impl Bound<bool> for bool {
    fn compare(&self, value: &bool) -> Option<Ordering> {
        value.partial_cmp(self)
    }
}

/// Whole-value tests on a JSON blob.
///
/// JSON is opaque to the filter: the shape belongs to whoever wrote the
/// manifest or the model that produced it, so there is nothing to compare
/// inside it that this schema could name. Equality of the whole value is
/// honest, and a path query would be a query language of its own.
// The scalar is `JSON`, so the filter is `JSONFilter` rather than the
// `Jsonfilter` the Rust name would PascalCase into.
#[derive(Clone, Debug, Default, InputObject)]
#[graphql(name = "JSONFilter")]
pub(crate) struct JSONFilter {
    /// Exactly this value.
    pub(crate) eq: Option<Json>,
    /// Anything but this value.
    pub(crate) ne: Option<Json>,
    /// Match where there is no value at all.
    pub(crate) is_null: Option<bool>,
}

impl JSONFilter {
    /// The comparisons this filter carries.
    pub(crate) fn parts(&self) -> Ordered<'_, Json> {
        Ordered::exact(self.eq.as_ref(), self.ne.as_ref(), None, None, self.is_null)
    }
}

impl Bound<Json> for Json {
    fn compare(&self, value: &Json) -> Option<Ordering> {
        // Two JSON values are equal or they are not; there is no order over
        // them to report. `eq` and `ne` read the equality, and the range
        // fields a JSON filter does not have would read nothing.
        (self == value).then_some(Ordering::Equal)
    }
}

/// Membership tests on a list of strings.
///
/// The quantifier inputs a mirrored type gets are about objects, where "some
/// item matches this filter" is the useful question. For a list of plain
/// strings it is membership, so this asks that directly.
#[derive(Clone, Debug, Default, InputObject)]
pub(crate) struct StringListFilter {
    /// Holds this string.
    pub(crate) has: Option<String>,
    /// Holds every one of these strings.
    pub(crate) has_every: Option<Vec<String>>,
    /// Holds at least one of these strings.
    pub(crate) has_some: Option<Vec<String>>,
    /// Holds nothing at all, or holds something.
    pub(crate) is_empty: Option<bool>,
    /// Match where there is no list at all.
    pub(crate) is_null: Option<bool>,
}

/// Whether a list of strings satisfies every membership test set on it.
///
/// Comparison is exact and case-sensitive, as `eq` on a single string is: a
/// list of names, tool ids or ancestor ids is a set of tokens rather than
/// prose.
pub(crate) fn string_list_test<S: AsRef<str>>(items: &[S], filter: &StringListFilter) -> bool {
    let holds = |wanted: &String| items.iter().any(|item| item.as_ref() == wanted.as_str());
    filter.is_null != Some(true)
        && filter.has.as_ref().is_none_or(holds)
        && filter
            .has_every
            .as_ref()
            .is_none_or(|wanted| wanted.iter().all(holds))
        && filter
            .has_some
            .as_ref()
            .is_none_or(|wanted| wanted.iter().any(holds))
        && filter
            .is_empty
            .is_none_or(|empty| items.is_empty() == empty)
}

/// Read `isNull` off each wire filter, for the one absent-value rule.
macro_rules! nullable {
    ($($filter:ty),+ $(,)?) => {
        $(impl Nullable for $filter {
            fn is_null(&self) -> Option<bool> {
                self.is_null
            }
        })+
    };
}

nullable!(
    StringFilter,
    IDFilter,
    IntFilter,
    BigIntFilter,
    FloatFilter,
    DecimalFilter,
    TimestampFilter,
    BooleanFilter,
    JSONFilter,
    StringListFilter,
);

#[cfg(test)]
#[path = "scalars_tests.rs"]
mod tests;
