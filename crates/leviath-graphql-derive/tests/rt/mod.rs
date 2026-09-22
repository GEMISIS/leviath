//! A runtime for the macro to compile against, standing in for the real one.
//!
//! The point of the `rt` argument is that the macro names a module and knows
//! nothing else about it. This is that module, written small: the same traits
//! and the same free functions, with the simplest bodies that are correct. If
//! the macro's output compiles and runs against this, it is calling the
//! contract rather than the implementation.

use std::future::Future;
use std::pin::Pin;

use async_graphql::InputType;

/// A future the runtime hands back from a trait method.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What one filter says about one value before anything is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    /// It does not match.
    No,
    /// It matches.
    Yes,
    /// A field that costs a read is still unanswered.
    Io,
}

impl Tri {
    /// The answer a plain boolean test gives.
    pub fn of(matched: bool) -> Self {
        if matched { Self::Yes } else { Self::No }
    }

    /// Both have to hold.
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Io, _) | (_, Self::Io) => Self::Io,
            (Self::Yes, Self::Yes) => Self::Yes,
        }
    }

    /// Either may hold.
    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::Io, _) | (_, Self::Io) => Self::Io,
            (Self::No, Self::No) => Self::No,
        }
    }

    /// The opposite answer.
    pub fn not(self) -> Self {
        match self {
            Self::No => Self::Yes,
            Self::Yes => Self::No,
            Self::Io => Self::Io,
        }
    }

    /// Every one of them has to hold.
    pub fn all(answers: impl Iterator<Item = Self>) -> Self {
        answers.fold(Self::Yes, Self::and)
    }

    /// At least one of them has to hold.
    pub fn any(answers: impl Iterator<Item = Self>) -> Self {
        answers.fold(Self::No, Self::or)
    }
}

/// What every test in one request shares.
#[derive(Debug, Default)]
pub struct MatchCx<'a> {
    /// The clock this request compares against.
    pub now: i64,
    /// What the stand-in borrows, so the lifetime is a real one.
    pub named: Option<&'a str>,
}

impl MatchCx<'_> {
    /// A context with a clock.
    pub fn at(now: i64) -> Self {
        Self { now, named: None }
    }
}

/// A value some GraphQL input can be tested against.
pub trait Filterable: Sync {
    /// The GraphQL input this value is filtered with.
    type Filter: InputType;

    /// What this filter says without reading anything.
    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri;

    /// The full answer, with whatever reads it takes.
    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool>;
}

/// A value a list of which can be filtered.
pub trait ListItem: Filterable + Sized {
    /// The GraphQL input a list of these is filtered with.
    type ListFilter: InputType;

    /// What this quantifier says without reading anything.
    fn list_test(items: &[Self], filter: &Self::ListFilter, cx: &MatchCx<'_>) -> Tri;

    /// The full answer for these items.
    fn list_confirm<'a>(
        items: &'a [Self],
        filter: &'a Self::ListFilter,
        cx: &'a MatchCx<'a>,
    ) -> BoxFuture<'a, bool>;
}

/// A filter input that carries `isNull`.
pub trait Nullable {
    /// What the client asked about the value being absent.
    fn is_null(&self) -> Option<bool>;
}

/// The generated input object that mirrors one output type.
pub trait Mirror: Sized + Send + Sync + Nullable {
    /// The output type this input mirrors.
    type Target: Sync;

    /// The combinators this filter carries.
    fn parts(&self) -> Parts<'_, Self>;

    /// Fold every field that can be answered from memory.
    fn cheap(&self, target: &Self::Target, cx: &MatchCx<'_>, acc: &mut Acc);

    /// Answer every field of this level, reading whatever that takes.
    fn io<'a>(&'a self, target: &'a Self::Target, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool>;
}

/// The combinators every mirror carries.
#[derive(Debug)]
pub struct Parts<'a, M> {
    /// Every filter here has to hold.
    pub and: Option<&'a [M]>,
    /// At least one filter here has to hold.
    pub or: Option<&'a [M]>,
    /// This filter must not hold.
    pub not: Option<&'a M>,
    /// What the client asked about absence.
    pub is_null: Option<bool>,
}

impl<'a, M> Parts<'a, M> {
    /// Gather the four combinators.
    pub fn new(
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
pub trait Quantified: Nullable + Sync {
    /// The item type the quantifiers are about.
    type Item: Filterable;

    /// The quantifiers this filter carries.
    fn quantifiers(&self) -> Quantifiers<'_, <Self::Item as Filterable>::Filter>;
}

/// The three quantifiers.
#[derive(Debug)]
pub struct Quantifiers<'a, F> {
    /// At least one item matches.
    pub some: Option<&'a F>,
    /// Every item matches.
    pub every: Option<&'a F>,
    /// No item matches.
    pub none: Option<&'a F>,
}

impl<'a, F> Quantifiers<'a, F> {
    /// Gather the three quantifiers.
    pub fn new(some: Option<&'a F>, every: Option<&'a F>, none: Option<&'a F>) -> Self {
        Self { some, every, none }
    }
}

/// The generated input object that compares an enum.
pub trait EnumParts: Nullable {
    /// The enum being compared.
    type Value: Copy + PartialEq;

    /// The comparisons this filter carries.
    fn choices(&self) -> Choices<'_, Self::Value>;
}

/// An enum filter's comparisons.
#[derive(Debug)]
pub struct Choices<'a, T> {
    /// Exactly this value.
    pub eq: Option<T>,
    /// Anything but this value.
    pub ne: Option<T>,
    /// One of these values.
    pub within: Option<&'a [T]>,
    /// None of these values.
    pub not_in: Option<&'a [T]>,
    /// What the client asked about absence.
    pub is_null: Option<bool>,
}

impl<'a, T> Choices<'a, T> {
    /// Gather the comparisons.
    pub fn new(
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

/// A field a listing may be ordered by.
pub trait OrderField: Copy + Eq {
    /// The name a cursor records for this field.
    fn wire(self) -> &'static str;
}

/// Something a listing can order by one of its own fields.
pub trait Orderable<Cx: ?Sized> {
    /// The fields this type may be ordered by.
    type Field: OrderField;

    /// This item's value for one order field.
    fn key(&self, field: Self::Field, cx: &Cx) -> CursorKey;
}

/// Which way a listing runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub enum OrderDirection {
    /// Smallest first.
    Asc,
    /// Largest first.
    Desc,
}

/// One sort key and the direction it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Term<F> {
    /// Which field.
    pub field: F,
    /// Which way.
    pub direction: OrderDirection,
}

/// One value in the keyset a cursor encodes.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorKey {
    /// A whole-number key.
    Int(i64),
    /// A text key.
    Text(String),
    /// The field had no value.
    Null,
}

/// A value that can be a sort key.
pub trait Sortable {
    /// This value as the cursor encodes it.
    fn cursor_key(&self) -> CursorKey;
}

/// The cursor key one orderable field's value sorts by.
pub fn sort_key<V: Sortable + ?Sized>(value: &V) -> CursorKey {
    value.cursor_key()
}

impl Sortable for str {
    fn cursor_key(&self) -> CursorKey {
        CursorKey::Text(self.to_owned())
    }
}

impl Sortable for String {
    fn cursor_key(&self) -> CursorKey {
        CursorKey::Text(self.clone())
    }
}

impl Sortable for i32 {
    fn cursor_key(&self) -> CursorKey {
        CursorKey::Int(i64::from(*self))
    }
}

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

/// The running answer a generated `cheap` folds into.
#[derive(Debug)]
pub struct Acc {
    /// The answer so far.
    verdict: Tri,
}

impl Acc {
    /// An accumulator that has seen nothing.
    pub fn new() -> Self {
        Self { verdict: Tri::Yes }
    }

    /// The answer after every field folded in.
    pub fn verdict(&self) -> Tri {
        self.verdict
    }

    /// Fold one answer in.
    fn fold(&mut self, answer: Tri) {
        if self.verdict != Tri::No {
            self.verdict = self.verdict.and(answer);
        }
    }

    /// The value is there, so `isNull: true` refuses it.
    pub fn present(&mut self, is_null: Option<bool>) {
        self.fold(Tri::of(is_null != Some(true)));
    }

    /// Test one cheap field.
    pub fn field<V: Filterable + ?Sized>(
        &mut self,
        filter: Option<&V::Filter>,
        value: &V,
        cx: &MatchCx<'_>,
    ) {
        if let Some(filter) = filter
            && self.verdict != Tri::No
        {
            self.fold(value.test(filter, cx));
        }
    }

    /// Note a field that costs a read, without reading it.
    pub fn pending<F>(&mut self, filter: &Option<F>) {
        if filter.is_some() {
            self.fold(Tri::Io);
        }
    }

    /// Test the variant a union value is, and refuse any other.
    pub fn variant<V: Filterable + ?Sized>(
        &mut self,
        filter: Option<&V::Filter>,
        value: &V,
        cx: &MatchCx<'_>,
        set: &[bool],
        at: usize,
    ) {
        let others = set
            .iter()
            .enumerate()
            .any(|(which, asked)| *asked && which != at);
        self.fold(Tri::of(!others));
        self.field(filter, value, cx);
    }
}

impl Default for Acc {
    fn default() -> Self {
        Self::new()
    }
}

/// The running answer a generated `io` folds into.
pub struct Confirm<'a> {
    /// Whether every field so far has matched.
    ok: bool,
    /// What the request shares.
    cx: &'a MatchCx<'a>,
}

impl<'a> Confirm<'a> {
    /// An answer that has seen nothing.
    pub fn new(cx: &'a MatchCx<'a>) -> Self {
        Self { ok: true, cx }
    }

    /// Answer one field.
    pub async fn field<V: Filterable + ?Sized>(&mut self, filter: Option<&V::Filter>, value: &V) {
        if let Some(filter) = filter
            && self.ok
        {
            self.ok = value.confirm(filter, self.cx).await;
        }
    }

    /// Answer one field that costs a read, reading it only if asked.
    pub async fn io<V: Filterable, F: Future<Output = V>>(
        &mut self,
        filter: Option<&V::Filter>,
        value: F,
    ) {
        if let Some(filter) = filter
            && self.ok
        {
            let value = value.await;
            self.ok = value.confirm(filter, self.cx).await;
        }
    }

    /// Answer the variant a union value is.
    pub async fn variant<V: Filterable + ?Sized>(
        &mut self,
        filter: Option<&V::Filter>,
        value: &V,
        set: &[bool],
        at: usize,
    ) {
        let others = set
            .iter()
            .enumerate()
            .any(|(which, asked)| *asked && which != at);
        self.ok = self.ok && !others;
        self.field(filter, value).await;
    }

    /// The answer after every field.
    pub fn finish(self) -> bool {
        self.ok
    }
}

/// A test that is already answered.
pub fn settled(answer: Tri) -> BoxFuture<'static, bool> {
    Box::pin(std::future::ready(answer == Tri::Yes))
}

/// What one mirror says about one value without reading anything.
pub fn object_test<M: Mirror>(value: &M::Target, filter: &M, cx: &MatchCx<'_>) -> Tri {
    let parts = filter.parts();
    let mut acc = Acc::new();
    acc.present(parts.is_null);
    filter.cheap(value, cx, &mut acc);
    let and = parts.and.map_or(Tri::Yes, |each| {
        Tri::all(each.iter().map(|one| object_test(value, one, cx)))
    });
    let or = parts.or.map_or(Tri::Yes, |each| {
        Tri::any(each.iter().map(|one| object_test(value, one, cx)))
    });
    let not = parts
        .not
        .map_or(Tri::Yes, |one| object_test(value, one, cx).not());
    acc.verdict().and(and).and(or).and(not)
}

/// What one mirror says about one value, reading whatever that takes.
pub fn object_confirm<'a, M: Mirror>(
    value: &'a M::Target,
    filter: &'a M,
    cx: &'a MatchCx<'a>,
) -> BoxFuture<'a, bool> {
    Box::pin(async move {
        let parts = filter.parts();
        if parts.is_null == Some(true) || !filter.io(value, cx).await {
            return false;
        }
        for one in parts.and.unwrap_or_default() {
            if !object_confirm(value, one, cx).await {
                return false;
            }
        }
        if let Some(each) = parts.or {
            let mut matched = false;
            for one in each {
                matched = matched || object_confirm(value, one, cx).await;
            }
            if !matched {
                return false;
            }
        }
        if let Some(one) = parts.not
            && object_confirm(value, one, cx).await
        {
            return false;
        }
        true
    })
}

/// What one enum filter says about one enum value.
pub fn enum_test<F: EnumParts>(value: &F::Value, filter: &F, _cx: &MatchCx<'_>) -> Tri {
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

/// What one quantifier input says about a list.
pub fn quantified<Q: Quantified>(items: &[Q::Item], filter: &Q, cx: &MatchCx<'_>) -> Tri {
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

/// What one quantifier input says about a list, with reads allowed.
pub fn quantified_confirm<'a, Q: Quantified>(
    items: &'a [Q::Item],
    filter: &'a Q,
    cx: &'a MatchCx<'a>,
) -> BoxFuture<'a, bool> {
    Box::pin(async move {
        let mut answer = filter.is_null() != Some(true);
        let quantifiers = filter.quantifiers();
        if let Some(each) = quantifiers.some {
            let mut any = false;
            for item in items {
                any = any || item.confirm(each, cx).await;
            }
            answer = answer && any;
        }
        if let Some(each) = quantifiers.every {
            for item in items {
                answer = answer && item.confirm(each, cx).await;
            }
        }
        if let Some(each) = quantifiers.none {
            for item in items {
                answer = answer && !item.confirm(each, cx).await;
            }
        }
        answer
    })
}

/// Exact tests on a string, which is all the stand-in needs.
#[derive(Clone, Debug, Default, async_graphql::InputObject)]
#[graphql(name = "MockStringFilter")]
pub struct StringFilter {
    /// Exactly this string.
    pub eq: Option<String>,
    /// Match where there is no string at all.
    pub is_null: Option<bool>,
}

impl Nullable for StringFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

/// Exact tests on a number.
#[derive(Clone, Debug, Default, async_graphql::InputObject)]
#[graphql(name = "MockIntFilter")]
pub struct IntFilter {
    /// Exactly this number.
    pub eq: Option<i32>,
    /// Match where there is no number at all.
    pub is_null: Option<bool>,
}

impl Nullable for IntFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

/// Membership tests on a list of strings.
#[derive(Clone, Debug, Default, async_graphql::InputObject)]
#[graphql(name = "MockStringListFilter")]
pub struct StringListFilter {
    /// Holds this string.
    pub has: Option<String>,
    /// Match where there is no list at all.
    pub is_null: Option<bool>,
}

impl Nullable for StringListFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

impl Filterable for str {
    type Filter = StringFilter;

    fn test(&self, filter: &Self::Filter, _cx: &MatchCx<'_>) -> Tri {
        Tri::of(filter.is_null != Some(true) && filter.eq.as_deref().is_none_or(|it| it == self))
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        settled(self.test(filter, cx))
    }
}

impl Filterable for String {
    type Filter = StringFilter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        self.as_str().test(filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        settled(self.test(filter, cx))
    }
}

impl Filterable for i32 {
    type Filter = IntFilter;

    fn test(&self, filter: &Self::Filter, _cx: &MatchCx<'_>) -> Tri {
        Tri::of(filter.is_null != Some(true) && filter.eq.is_none_or(|it| it == *self))
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        settled(self.test(filter, cx))
    }
}

impl<T: Filterable + ?Sized> Filterable for &T {
    type Filter = T::Filter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        (*self).test(filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        (*self).confirm(filter, cx)
    }
}

impl<T: Filterable> Filterable for Option<T>
where
    T::Filter: Nullable,
{
    type Filter = T::Filter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        match self {
            None => Tri::of(filter.is_null() == Some(true)),
            Some(value) => value.test(filter, cx),
        }
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        match self {
            None => settled(Tri::of(filter.is_null() == Some(true))),
            Some(value) => value.confirm(filter, cx),
        }
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

impl ListItem for String {
    type ListFilter = StringListFilter;

    fn list_test(items: &[Self], filter: &Self::ListFilter, _cx: &MatchCx<'_>) -> Tri {
        Tri::of(
            filter.is_null != Some(true)
                && filter
                    .has
                    .as_ref()
                    .is_none_or(|wanted| items.contains(wanted)),
        )
    }

    fn list_confirm<'a>(
        items: &'a [Self],
        filter: &'a Self::ListFilter,
        cx: &'a MatchCx<'a>,
    ) -> BoxFuture<'a, bool> {
        settled(Self::list_test(items, filter, cx))
    }
}
