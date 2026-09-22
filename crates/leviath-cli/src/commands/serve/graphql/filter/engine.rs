//! Every decision the filter system makes.
//!
//! A generated impl calls into this file and does nothing else, so the rules
//! below are written once for the whole schema: how the combinators compose,
//! when a cheap answer is final, and what the second phase is allowed to
//! revisit.
//!
//! The two phases have to agree. [`object_test`] may answer `Io`, and then
//! [`object_confirm`] gives the real answer; but where `object_test` says
//! `Yes` or `No`, `object_confirm` would say the same thing. That is what lets
//! a listing drop a run without opening a single one of its files, and it
//! holds underneath `not` and `or` because both phases apply the same
//! combinator to the same sub-answers.

use super::{Acc, BoxFuture, EnumParts, Filterable, MatchCx, Mirror, Quantified, Tri};

/// A test that is already answered, as the future a caller expects.
pub(crate) fn settled(answer: Tri) -> BoxFuture<'static, bool> {
    Box::pin(std::future::ready(answer == Tri::Yes))
}

/// What one mirror says about one value without reading anything.
pub(crate) fn object_test<M: Mirror>(value: &M::Target, filter: &M, cx: &MatchCx<'_>) -> Tri {
    let parts = filter.parts();
    let mut acc = Acc::new();
    acc.present(parts.is_null);
    filter.cheap(value, cx, &mut acc);

    let and = parts.and.map_or(Tri::Yes, |filters| {
        Tri::all(filters.iter().map(|each| object_test(value, each, cx)))
    });
    let or = parts.or.map_or(Tri::Yes, |filters| {
        Tri::any(filters.iter().map(|each| object_test(value, each, cx)))
    });
    let not = parts
        .not
        .map_or(Tri::Yes, |each| object_test(value, each, cx).not());

    acc.verdict().and(and).and(or).and(not)
}

/// What one mirror says about one value, reading whatever that takes.
pub(crate) fn object_confirm<'a, M: Mirror>(
    value: &'a M::Target,
    filter: &'a M,
    cx: &'a MatchCx<'a>,
) -> BoxFuture<'a, bool> {
    Box::pin(async move {
        let parts = filter.parts();
        if parts.is_null == Some(true) {
            return false;
        }
        if !filter.io(value, cx).await {
            return false;
        }
        for each in parts.and.unwrap_or_default() {
            if !object_confirm(value, each, cx).await {
                return false;
            }
        }
        if let Some(filters) = parts.or {
            let mut matched = false;
            for each in filters {
                matched = object_confirm(value, each, cx).await;
                if matched {
                    break;
                }
            }
            if !matched {
                return false;
            }
        }
        if let Some(each) = parts.not
            && object_confirm(value, each, cx).await
        {
            return false;
        }
        true
    })
}

/// What one enum filter says about one enum value.
pub(crate) fn enum_test<F: EnumParts>(value: &F::Value, filter: &F, _cx: &MatchCx<'_>) -> Tri {
    let choices = filter.choices();
    Tri::of(
        choices.is_null != Some(true)
            && choices.eq.is_none_or(|wanted| wanted == *value)
            && choices.ne.is_none_or(|wanted| wanted != *value)
            && choices
                .within
                .is_none_or(|set| set.iter().any(|wanted| wanted == value))
            && choices
                .not_in
                .is_none_or(|set| !set.iter().any(|wanted| wanted == value)),
    )
}

/// What one quantifier input says about a list, without reading anything.
pub(crate) fn quantified<Q: Quantified>(items: &[Q::Item], filter: &Q, cx: &MatchCx<'_>) -> Tri {
    let quantifiers = filter.quantifiers();
    let some = quantifiers.some.map_or(Tri::Yes, |each| {
        Tri::any(items.iter().map(|item| item.test(each, cx)))
    });
    let every = quantifiers.every.map_or(Tri::Yes, |each| {
        Tri::all(items.iter().map(|item| item.test(each, cx)))
    });
    let none = quantifiers.none.map_or(Tri::Yes, |each| {
        Tri::any(items.iter().map(|item| item.test(each, cx))).not()
    });

    Tri::of(filter.is_null() != Some(true))
        .and(some)
        .and(every)
        .and(none)
}

/// What one quantifier input says about a list, reading whatever that takes.
pub(crate) fn quantified_confirm<'a, Q: Quantified>(
    items: &'a [Q::Item],
    filter: &'a Q,
    cx: &'a MatchCx<'a>,
) -> BoxFuture<'a, bool> {
    Box::pin(async move {
        let quantifiers = filter.quantifiers();
        if filter.is_null() == Some(true) {
            return false;
        }
        if let Some(each) = quantifiers.some
            && !any_item(items, each, cx).await
        {
            return false;
        }
        if let Some(each) = quantifiers.every
            && !every_item(items, each, cx).await
        {
            return false;
        }
        if let Some(each) = quantifiers.none
            && any_item(items, each, cx).await
        {
            return false;
        }
        true
    })
}

/// Whether at least one item matches, stopping at the first that does.
async fn any_item<T: Filterable>(items: &[T], filter: &T::Filter, cx: &MatchCx<'_>) -> bool {
    for item in items {
        if item.confirm(filter, cx).await {
            return true;
        }
    }
    false
}

/// Whether every item matches, stopping at the first that does not.
async fn every_item<T: Filterable>(items: &[T], filter: &T::Filter, cx: &MatchCx<'_>) -> bool {
    for item in items {
        if !item.confirm(filter, cx).await {
            return false;
        }
    }
    true
}

/// The running answer a generated `io` folds one field at a time into.
///
/// It is the second phase's counterpart to [`Acc`]: the same list of fields,
/// with reads allowed. Once a field has said no, the fields after it are not
/// evaluated, which is what keeps a filter that names a file behind a cheap
/// field that already failed from ever opening that file.
pub(crate) struct Confirm<'a> {
    /// Whether every field so far has matched.
    ok: bool,
    /// What the whole request shares.
    cx: &'a MatchCx<'a>,
}

impl<'a> Confirm<'a> {
    /// An answer that has seen nothing, and so far agrees.
    pub(crate) fn new(cx: &'a MatchCx<'a>) -> Self {
        Self { ok: true, cx }
    }

    /// Answer one field against the filter set for it, if one was set.
    pub(crate) async fn field<V>(&mut self, filter: Option<&V::Filter>, value: &V)
    where
        V: Filterable + ?Sized,
    {
        if let Some(filter) = filter
            && self.ok
        {
            self.ok = value.confirm(filter, self.cx).await;
        }
    }

    /// Answer one field that costs a read, reading it only if it was asked
    /// about and nothing before it has already said no.
    pub(crate) async fn io<V, F>(&mut self, filter: Option<&V::Filter>, value: F)
    where
        V: Filterable,
        F: std::future::Future<Output = V>,
    {
        if let Some(filter) = filter
            && self.ok
        {
            let value = value.await;
            self.ok = value.confirm(filter, self.cx).await;
        }
    }

    /// Answer the variant a union value actually is, and refuse any other.
    pub(crate) async fn variant<V>(
        &mut self,
        filter: Option<&V::Filter>,
        value: &V,
        set: &[bool],
        at: usize,
    ) where
        V: Filterable + ?Sized,
    {
        let others = set
            .iter()
            .enumerate()
            .any(|(which, asked)| *asked && which != at);
        self.ok = self.ok && !others;
        self.field(filter, value).await;
    }

    /// The answer after every field a generated `io` folded in.
    pub(crate) fn finish(self) -> bool {
        self.ok
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
