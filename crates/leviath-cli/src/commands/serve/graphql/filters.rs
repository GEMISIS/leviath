//! The scalar filters every filter input in this schema is built from.
//!
//! One input object per scalar, reused wherever a field of that scalar can be
//! filtered. A client that has learnt `StringFilter` on a run's title knows it
//! on a blueprint's description, and a field that becomes filterable adds a
//! line rather than a type.
//!
//! Each input compiles into a plain matcher - [`Text`], [`Ordered`],
//! [`Flag`] - before any run is read. Compiling once per request is what lets
//! a narrow wire type widen into the type the value is actually read as: a
//! `Int` bound and an age in seconds compare as `i64` rather than at the edge
//! of a 32-bit number.
//!
//! Every field set in one filter object has to hold. Leaving a field out is
//! not a wildcard match on that field, it is the absence of that comparison.

use async_graphql::InputObject;

use super::scalars::{Decimal, Timestamp};

/// The comparisons an ordered filter makes, in the type the value is read as.
///
/// `Debug` is what the cursor's filter digest is taken over, so the derive is
/// load-bearing: the digest only has to be a deterministic function of the
/// compiled filter, and the derived rendering is exactly that.
#[derive(Debug, PartialEq)]
pub(crate) struct Ordered<T> {
    /// Equal to this.
    pub(crate) eq: Option<T>,
    /// Not equal to this.
    pub(crate) ne: Option<T>,
    /// One of these.
    pub(crate) within: Option<Vec<T>>,
    /// None of these.
    pub(crate) outside: Option<Vec<T>>,
    /// Strictly below this.
    pub(crate) lt: Option<T>,
    /// At or below this.
    pub(crate) lte: Option<T>,
    /// Strictly above this.
    pub(crate) gt: Option<T>,
    /// At or above this.
    pub(crate) gte: Option<T>,
}

impl<T: PartialOrd> Ordered<T> {
    /// Whether a value satisfies every comparison this filter carries.
    pub(crate) fn matches(&self, value: &T) -> bool {
        self.eq.as_ref().is_none_or(|bound| value == bound)
            && self.ne.as_ref().is_none_or(|bound| value != bound)
            && self
                .within
                .as_ref()
                .is_none_or(|set| set.iter().any(|bound| value == bound))
            && self
                .outside
                .as_ref()
                .is_none_or(|set| !set.iter().any(|bound| value == bound))
            && self.lt.as_ref().is_none_or(|bound| value < bound)
            && self.lte.as_ref().is_none_or(|bound| value <= bound)
            && self.gt.as_ref().is_none_or(|bound| value > bound)
            && self.gte.as_ref().is_none_or(|bound| value >= bound)
    }

    /// Whether a value the run may not have satisfies this filter.
    ///
    /// A run with no value for the field satisfies nothing: the comparison is
    /// about a number that is not there. `not` around the filter is how a
    /// client asks for those runs.
    pub(crate) fn matches_option(&self, value: Option<T>) -> bool {
        value.is_some_and(|value| self.matches(&value))
    }
}

/// The comparisons a text filter makes.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Text {
    /// Equal to this, case-sensitively.
    pub(crate) eq: Option<String>,
    /// Not equal to this, case-sensitively.
    pub(crate) ne: Option<String>,
    /// One of these, case-sensitively.
    pub(crate) within: Option<Vec<String>>,
    /// None of these, case-sensitively.
    pub(crate) outside: Option<Vec<String>>,
    /// Holds this somewhere, ignoring ASCII case.
    pub(crate) contains: Option<String>,
    /// Begins with this, ignoring ASCII case.
    pub(crate) starts_with: Option<String>,
    /// Ends with this, ignoring ASCII case.
    pub(crate) ends_with: Option<String>,
}

impl Text {
    /// Whether a string satisfies every comparison this filter carries.
    pub(crate) fn matches(&self, value: &str) -> bool {
        let folded = value.to_ascii_lowercase();
        self.eq.as_deref().is_none_or(|bound| value == bound)
            && self.ne.as_deref().is_none_or(|bound| value != bound)
            && self
                .within
                .as_ref()
                .is_none_or(|set| set.iter().any(|bound| bound == value))
            && self
                .outside
                .as_ref()
                .is_none_or(|set| !set.iter().any(|bound| bound == value))
            && self
                .contains
                .as_deref()
                .is_none_or(|bound| folded.contains(&bound.to_ascii_lowercase()))
            && self
                .starts_with
                .as_deref()
                .is_none_or(|bound| folded.starts_with(&bound.to_ascii_lowercase()))
            && self
                .ends_with
                .as_deref()
                .is_none_or(|bound| folded.ends_with(&bound.to_ascii_lowercase()))
    }

    /// Whether a string the run may not have satisfies this filter.
    ///
    /// A run with no value for the field satisfies nothing, for the same
    /// reason a number that is not there satisfies no comparison. `not` around
    /// the filter is how a client asks for those runs.
    pub(crate) fn matches_option(&self, value: Option<&str>) -> bool {
        value.is_some_and(|value| self.matches(value))
    }
}

/// The comparisons a boolean filter makes.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Flag {
    /// Equal to this.
    pub(crate) eq: Option<bool>,
    /// Not equal to this.
    pub(crate) ne: Option<bool>,
}

impl Flag {
    /// Whether a boolean satisfies every comparison this filter carries.
    pub(crate) fn matches(&self, value: bool) -> bool {
        self.eq.is_none_or(|bound| value == bound) && self.ne.is_none_or(|bound| value != bound)
    }
}

/// Text comparisons on one field.
///
/// `eq`, `ne`, `in` and `notIn` compare the whole string exactly, case
/// included. `contains`, `startsWith` and `endsWith` ignore ASCII case, as the
/// run search does. Every field set here has to hold.
#[derive(Debug, Default, InputObject)]
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
}

impl StringFilter {
    /// The matcher this filter runs as.
    pub(crate) fn compiled(self) -> Text {
        Text {
            eq: self.eq,
            ne: self.ne,
            within: self.within,
            outside: self.not_in,
            contains: self.contains,
            starts_with: self.starts_with,
            ends_with: self.ends_with,
        }
    }
}

/// Comparisons on one whole-number field.
///
/// The range fields are what "older than", "ran longer than" and every bound
/// between are asked with. Every field set here has to hold, so `gte` and `lt`
/// together are a half-open range.
#[derive(Debug, Default, InputObject)]
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
}

impl IntFilter {
    /// The matcher this filter runs as, widened to the 64 bits the values are
    /// read as.
    pub(crate) fn compiled(self) -> Ordered<i64> {
        Ordered {
            eq: self.eq.map(i64::from),
            ne: self.ne.map(i64::from),
            within: self.within.map(widen),
            outside: self.not_in.map(widen),
            lt: self.lt.map(i64::from),
            lte: self.lte.map(i64::from),
            gt: self.gt.map(i64::from),
            gte: self.gte.map(i64::from),
        }
    }
}

/// Widen a list of wire-sized numbers to the type values are compared in.
fn widen(numbers: Vec<i32>) -> Vec<i64> {
    numbers.into_iter().map(i64::from).collect()
}

/// Comparisons on one exact-decimal field, such as spend.
///
/// The bounds travel as decimal strings, the way every `Decimal` in this
/// schema does, so a cost bound a JSON parser would re-round is written out
/// rather than rounded. Every field set here has to hold.
#[derive(Debug, Default, InputObject)]
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
}

impl DecimalFilter {
    /// The matcher this filter runs as.
    pub(crate) fn compiled(self) -> Ordered<Decimal> {
        Ordered {
            eq: self.eq,
            ne: self.ne,
            within: self.within,
            outside: self.not_in,
            lt: self.lt,
            lte: self.lte,
            gt: self.gt,
            gte: self.gte,
        }
    }
}

/// Comparisons on one timestamp field, in unix epoch seconds.
///
/// "Started before this", "touched since that" and every window between.
/// Every field set here has to hold, so `gte` and `lt` together are a
/// half-open window.
#[derive(Debug, Default, InputObject)]
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
}

impl TimestampFilter {
    /// The matcher this filter runs as, in the seconds a run records.
    pub(crate) fn compiled(self) -> Ordered<i64> {
        Ordered {
            eq: self.eq.map(|at| at.0),
            ne: self.ne.map(|at| at.0),
            within: self.within.map(seconds),
            outside: self.not_in.map(seconds),
            lt: self.lt.map(|at| at.0),
            lte: self.lte.map(|at| at.0),
            gt: self.gt.map(|at| at.0),
            gte: self.gte.map(|at| at.0),
        }
    }
}

/// The seconds behind a list of timestamps.
fn seconds(stamps: Vec<Timestamp>) -> Vec<i64> {
    stamps.into_iter().map(|at| at.0).collect()
}

/// Comparisons on one boolean field.
///
/// Two fields rather than one because `ne` reads as itself beside every other
/// filter in the schema. `eq: false` and `ne: true` select the same runs.
#[derive(Debug, Default, InputObject)]
pub(crate) struct BooleanFilter {
    /// Exactly this.
    pub(crate) eq: Option<bool>,
    /// Anything but this.
    pub(crate) ne: Option<bool>,
}

impl BooleanFilter {
    /// The matcher this filter runs as.
    pub(crate) fn compiled(self) -> Flag {
        Flag {
            eq: self.eq,
            ne: self.ne,
        }
    }
}

#[cfg(test)]
#[path = "filters_tests.rs"]
mod tests;
