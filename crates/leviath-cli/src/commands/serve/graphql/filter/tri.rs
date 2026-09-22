//! Three-valued logic, and the accumulator a generated mirror folds into.
//!
//! A filter is answered in two phases. The first reads only what is already in
//! memory; the second is allowed to open files. [`Tri`] is what the first phase
//! returns: `No` and `Yes` are final answers, and `Io` means "everything cheap
//! agreed so far, and something that costs a read is still unanswered".
//!
//! The rule the whole listing depends on is that a final answer is the true
//! answer. `No` is never revised into a match by the second phase, so a run the
//! cheap pass dropped is never read from disk, and that holds underneath `not`
//! and `or` as much as at the top level.

use super::{Filterable, MatchCx};

/// What one filter says about one value before any file is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tri {
    /// It does not match, and no read can change that.
    No,
    /// It matches, and no read can change that.
    Yes,
    /// Undecided: a field that costs a read has to be looked at.
    Io,
}

impl Tri {
    /// The answer a plain boolean test gives.
    pub(crate) fn of(matched: bool) -> Self {
        if matched { Self::Yes } else { Self::No }
    }

    /// Both have to hold.
    ///
    /// `No` wins over everything, which is what keeps a definite refusal
    /// definite when an undecided field sits beside it.
    pub(crate) fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Io, _) | (_, Self::Io) => Self::Io,
            (Self::Yes, Self::Yes) => Self::Yes,
        }
    }

    /// Either may hold.
    ///
    /// `Yes` wins over everything, so an alternative that already matched is
    /// not paid for with a read of the one beside it.
    pub(crate) fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::Io, _) | (_, Self::Io) => Self::Io,
            (Self::No, Self::No) => Self::No,
        }
    }

    /// The opposite answer. An undecided answer stays undecided.
    pub(crate) fn not(self) -> Self {
        match self {
            Self::No => Self::Yes,
            Self::Yes => Self::No,
            Self::Io => Self::Io,
        }
    }

    /// Every one of them has to hold. Nothing at all holds.
    pub(crate) fn all(answers: impl Iterator<Item = Self>) -> Self {
        answers.fold(Self::Yes, Self::and)
    }

    /// At least one of them has to hold. Nothing at all holds nothing.
    pub(crate) fn any(answers: impl Iterator<Item = Self>) -> Self {
        answers.fold(Self::No, Self::or)
    }
}

/// The running answer a generated `cheap` folds one field at a time into.
///
/// Every method here is what a generated line calls; none of them decides
/// anything a person has to re-read per type, which is the point. Once the
/// answer is `No` the rest of the fields are not evaluated at all.
#[derive(Debug)]
pub(crate) struct Acc {
    /// The answer so far, with every field seen ANDed together.
    verdict: Tri,
}

impl Default for Acc {
    fn default() -> Self {
        Self::new()
    }
}

impl Acc {
    /// An accumulator that has seen nothing, and so far agrees.
    pub(crate) fn new() -> Self {
        Self { verdict: Tri::Yes }
    }

    /// The answer after every field a generated `cheap` folded in.
    pub(crate) fn verdict(&self) -> Tri {
        self.verdict
    }

    /// Fold one answer in, unless the answer is already settled at `No`.
    fn fold(&mut self, answer: Tri) {
        if self.verdict != Tri::No {
            self.verdict = self.verdict.and(answer);
        }
    }

    /// The value this filter is about is there, so `isNull: true` refuses it.
    ///
    /// `isNull: false` is the opposite ask and holds here, and an unset
    /// `isNull` says nothing either way. The missing-value half of the rule
    /// lives in `values::option_test`, which is the only place a value can be
    /// absent at all.
    pub(crate) fn present(&mut self, is_null: Option<bool>) {
        self.fold(Tri::of(is_null != Some(true)));
    }

    /// Test one cheap field against the filter set for it, if one was set.
    pub(crate) fn field<V>(&mut self, filter: Option<&V::Filter>, value: &V, cx: &MatchCx<'_>)
    where
        V: Filterable + ?Sized,
    {
        if self.verdict == Tri::No {
            return;
        }
        if let Some(filter) = filter {
            self.fold(value.test(filter, cx));
        }
    }

    /// Note that a field costing a read was asked about, without reading it.
    pub(crate) fn pending<F>(&mut self, filter: &Option<F>) {
        if filter.is_some() {
            self.fold(Tri::Io);
        }
    }

    /// Test the variant a union value actually is, and refuse any other.
    ///
    /// `set` says which variant fields the filter carries, in declaration
    /// order, and `at` is the position of the one this value is. A filter on a
    /// variant the value is not never matches, which is what makes a union
    /// mirror a test of the variant as well as of its contents.
    pub(crate) fn variant<V>(
        &mut self,
        filter: Option<&V::Filter>,
        value: &V,
        cx: &MatchCx<'_>,
        set: &[bool],
        at: usize,
    ) where
        V: Filterable + ?Sized,
    {
        let others = set
            .iter()
            .enumerate()
            .any(|(which, asked)| *asked && which != at);
        self.fold(Tri::of(!others));
        self.field(filter, value, cx);
    }
}

#[cfg(test)]
#[path = "tri_tests.rs"]
mod tests;
