//! Which filter each kind of value is tested with.
//!
//! One `Filterable` impl per scalar, plus the four wrappers a resolver return
//! type is actually made of: a reference, an `Option`, a list and a `Result`.
//! A mirrored field's type goes through these until it reaches a scalar or
//! another mirrored type, which is why a field becomes filterable by being
//! written rather than by being registered anywhere.
//!
//! `Option<T>` is the one place a value can be absent, so it is the one place
//! `isNull` is answered.

use async_graphql::ID;

use super::super::scalars::{BigInt, Decimal, Json, Timestamp};
use super::scalars::{
    BigIntFilter, BooleanFilter, DecimalFilter, FloatFilter, IDFilter, IntFilter, JSONFilter,
    StringFilter, StringListFilter, TimestampFilter, exact_test, ordered_test, string_list_test,
    text_test,
};
use super::{
    BoxFuture, CursorKey, Filterable, ListItem, MatchCx, Nullable, Sortable, Tri, settled,
};

/// A scalar whose filter compares it directly, with no wrapper in between.
///
/// The three lines are the same for every scalar, so they are written once
/// here rather than nine times below: the filter, the test, and the answer
/// that a settled test needs no reads to confirm.
macro_rules! scalar {
    ($value:ty, $filter:ty, $test:expr) => {
        impl Filterable for $value {
            type Filter = $filter;

            fn test(&self, filter: &Self::Filter, _cx: &MatchCx<'_>) -> Tri {
                let matched: fn(&$value, &$filter) -> bool = $test;
                Tri::of(matched(self, filter))
            }

            fn confirm<'a>(
                &'a self,
                filter: &'a Self::Filter,
                cx: &'a MatchCx<'a>,
            ) -> BoxFuture<'a, bool> {
                settled(self.test(filter, cx))
            }
        }
    };
}

scalar!(str, StringFilter, text_test);
scalar!(String, StringFilter, |value, filter| text_test(
    value, filter
));
scalar!(ID, IDFilter, |value, filter| exact_test(
    value,
    &filter.parts()
));
scalar!(i32, IntFilter, |value, filter| ordered_test(
    value,
    &filter.parts()
));
scalar!(i64, BigIntFilter, |value, filter| ordered_test(
    value,
    &filter.parts()
));
scalar!(BigInt, BigIntFilter, |value, filter| ordered_test(
    value,
    &filter.parts()
));
scalar!(f64, FloatFilter, |value, filter| ordered_test(
    value,
    &filter.parts()
));
scalar!(Decimal, DecimalFilter, |value, filter| ordered_test(
    value,
    &filter.parts()
));
scalar!(Timestamp, TimestampFilter, |value, filter| ordered_test(
    value,
    &filter.parts()
));
scalar!(bool, BooleanFilter, |value, filter| exact_test(
    value,
    &filter.parts()
));
scalar!(Json, JSONFilter, |value, filter| exact_test(
    value,
    &filter.parts()
));

impl<T: Filterable + ?Sized> Filterable for &T {
    type Filter = T::Filter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        (*self).test(filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        (*self).confirm(filter, cx)
    }
}

/// Whether a value that may not be there satisfies a filter.
///
/// This is the whole of the `isNull` rule. A value that is not there matches
/// only `isNull: true`, and nothing else in the filter is even looked at,
/// because every other field is a comparison against a value there is none of.
/// A value that is there never matches `isNull: true`.
pub(crate) fn option_test<T>(value: Option<&T>, filter: &T::Filter, cx: &MatchCx<'_>) -> Tri
where
    T: Filterable,
    T::Filter: Nullable,
{
    match value {
        None => Tri::of(filter.is_null() == Some(true)),
        Some(value) => value.test(filter, cx),
    }
}

impl<T> Filterable for Option<T>
where
    T: Filterable,
    T::Filter: Nullable,
{
    type Filter = T::Filter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        option_test(self.as_ref(), filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        match self {
            None => settled(Tri::of(filter.is_null() == Some(true))),
            Some(value) => value.confirm(filter, cx),
        }
    }
}

impl<T, E> Filterable for Result<T, E>
where
    T: Filterable,
    E: Sync,
{
    type Filter = T::Filter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        // A field that failed to be read has no value to compare, and saying
        // "matches" would put a run in a listing on the strength of an error.
        self.as_ref()
            .map_or(Tri::No, |value| value.test(filter, cx))
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        match self {
            Err(_) => settled(Tri::No),
            Ok(value) => value.confirm(filter, cx),
        }
    }
}

impl<T: ListItem> Filterable for [T] {
    type Filter = T::ListFilter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        T::list_test(self, filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        T::list_confirm(self, filter, cx)
    }
}

impl<T: ListItem> Filterable for Vec<T> {
    type Filter = T::ListFilter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        T::list_test(self, filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        T::list_confirm(self, filter, cx)
    }
}

/// A list of strings, which is asked about membership rather than quantifiers.
macro_rules! string_list {
    ($item:ty) => {
        impl ListItem for $item {
            type ListFilter = StringListFilter;

            fn list_test(items: &[Self], filter: &Self::ListFilter, _cx: &MatchCx<'_>) -> Tri {
                Tri::of(string_list_test(items, filter))
            }

            fn list_confirm<'a>(
                items: &'a [Self],
                filter: &'a Self::ListFilter,
                cx: &'a MatchCx<'a>,
            ) -> BoxFuture<'a, bool> {
                settled(Self::list_test(items, filter, cx))
            }
        }
    };
}

string_list!(String);
string_list!(&str);
string_list!(ID);

/// A value that is its own sort key, in the form the cursor records.
macro_rules! sortable {
    ($value:ty, $key:expr) => {
        impl Sortable for $value {
            fn cursor_key(&self) -> CursorKey {
                let key: fn(&$value) -> CursorKey = $key;
                key(self)
            }
        }
    };
}

sortable!(str, |value| CursorKey::Text(value.to_owned()));
sortable!(String, |value| CursorKey::Text(value.clone()));
sortable!(ID, |value| CursorKey::Text(value.to_string()));
sortable!(i32, |value| CursorKey::Int(i64::from(*value)));
sortable!(i64, |value| CursorKey::Int(*value));
sortable!(BigInt, |value| CursorKey::Int(value.0));
sortable!(Timestamp, |value| CursorKey::Int(value.0));

impl<T: Sortable + ?Sized> Sortable for &T {
    fn cursor_key(&self) -> CursorKey {
        (*self).cursor_key()
    }
}

impl<T: Sortable> Sortable for Option<T> {
    fn cursor_key(&self) -> CursorKey {
        self.as_ref().map_or(CursorKey::Null, T::cursor_key)
    }
}

#[cfg(test)]
#[path = "values_tests.rs"]
mod tests;
