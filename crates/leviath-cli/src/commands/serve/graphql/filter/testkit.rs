//! One call that runs every function `#[mirror]` wrote for a type.
//!
//! The generated impls are straight lines of delegation, so running each of
//! them once is enough to measure all of them. That is what this is for: a
//! converted type file gets one test per mirrored type, and the coverage gate
//! is satisfied by the type's own test rather than by whichever query happens
//! to select the right fields.
//!
//! Pass one value per shape the type can take. For a struct or a resolver impl
//! that is a single value; for a union it is one value per variant, because
//! the generated code matches on the variant.

use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

use super::{
    Acc, EnumParts, Filterable, ListItem, MatchCx, Mirror, Nullable, OrderField, Orderable,
    Quantified, Tri,
};

/// Values that are the wrong type for almost anything.
///
/// Tried in turn until one is refused, because what is wrong depends on the
/// field: a number is not a string, a string is not a number or a nested
/// filter, and an object is not either of those.
fn wrong_values() -> [Value; 4] {
    [
        Value::Number(1.into()),
        Value::String("wrong".to_string()),
        Value::Boolean(true),
        Value::Object(IndexMap::new()),
    ]
}

/// One object carrying one field.
fn one(name: &str, value: Value) -> Value {
    let mut map = IndexMap::new();
    map.insert(Name::new(name), value);
    Value::Object(map)
}

/// The whole object the type wrote, with one field spoiled.
///
/// The counterpart of [`one`], and the half that reaches the later fields: a
/// derived reader takes the fields in declaration order and stops at the first
/// one it cannot read, so an object carrying nothing but a bad third field is
/// refused for the missing first one and the third is never looked at. Sending
/// everything, with one field wrong, is what walks the rest.
fn instead(whole: &serde_json::Map<String, serde_json::Value>, name: &str, value: Value) -> Value {
    let mut spoiled = whole.clone();
    spoiled.insert(name.to_string(), value.into_json().unwrap_or_default());
    Value::from_json(serde_json::Value::Object(spoiled)).unwrap_or_default()
}

/// Read one filter off the wire the way a request delivers it, whole and one
/// bad field at a time.
///
/// async-graphql's derive reads an input object's fields in declaration order,
/// and gives each field its own error path. A mistake in the first field is
/// refused before any of the others are read, so only a mistake in a *later*
/// field reaches the paths after it. Driving every field in turn is what
/// measures all of them, and it is why this lives here rather than in each
/// converted file: a type gains a field and its test does not have to change.
///
/// A field that takes any value at all, such as a `JSON` one, is simply never
/// refused, and that is not a failure.
pub(crate) fn round_trip<F: InputType>(filter: &F) {
    let whole = filter.to_value();
    assert!(
        F::parse(Some(whole.clone())).is_ok(),
        "a filter reads back what it wrote"
    );
    assert!(
        F::parse(Some(Value::Boolean(true))).is_err(),
        "a filter is an object, and a bare value is not one"
    );

    let json = whole.into_json().unwrap_or_default();
    let fields = json.as_object().cloned().unwrap_or_default();
    let mut refused = 0usize;
    for name in fields.keys() {
        // Both shapes, because they reach different halves of the reader: the
        // field on its own, and the field spoiled inside an otherwise sound
        // object. See [`instead`].
        let alone = wrong_values()
            .into_iter()
            .any(|value| F::parse(Some(one(name, value))).is_err());
        let beside = wrong_values()
            .into_iter()
            .any(|value| F::parse(Some(instead(&fields, name, value))).is_err());
        refused += usize::from(alone);
        refused += usize::from(beside);
    }
    assert!(refused > 0, "a filter refuses a field of the wrong type");
}

/// Run every function the macro wrote for one mirrored type.
///
/// This is an `async fn` on purpose: the confirm phase is asynchronous, and a
/// test that drives it wants to be a `#[tokio::test]` rather than to build a
/// runtime of its own inside one.
pub(crate) async fn exercise<T>(values: &[T])
where
    T: Filterable,
    T::Filter: Mirror<Target = T> + Default,
{
    let filter = T::Filter::default();
    let cx = MatchCx::at(0);
    round_trip(&filter);

    assert_eq!(filter.is_null(), None, "a default filter asks nothing");
    let parts = filter.parts();
    assert!(
        parts.and.is_none() && parts.or.is_none() && parts.not.is_none(),
        "a default filter carries no combinators"
    );

    for value in values {
        let mut acc = Acc::new();
        filter.cheap(value, &cx, &mut acc);
        assert_eq!(
            value.test(&filter, &cx),
            acc.verdict(),
            "a filter that asks nothing is answered by the fields alone"
        );
        assert!(
            value.confirm(&filter, &cx).await,
            "a filter that asks nothing matches everything"
        );
    }
}

/// Run every function the macro wrote for one mirrored enum.
///
/// An enum's comparator is not a mirror: it has no fields of its own to fold,
/// so it carries `EnumParts` instead of `Mirror` and needs its own call.
pub(crate) async fn exercise_enum<T>(values: &[T])
where
    T: Filterable,
    T::Filter: EnumParts<Value = T> + Default,
{
    let filter = T::Filter::default();
    let cx = MatchCx::at(0);
    round_trip(&filter);

    assert_eq!(filter.is_null(), None, "a default comparator asks nothing");
    let choices = filter.choices();
    assert!(
        choices.eq.is_none() && choices.ne.is_none(),
        "a default comparator carries no comparisons"
    );

    for value in values {
        assert_eq!(
            value.test(&filter, &cx),
            Tri::Yes,
            "a comparator that asks nothing matches every value"
        );
        assert!(value.confirm(&filter, &cx).await);
    }
}

/// Run every function the macro wrote for one type's list input.
pub(crate) async fn exercise_list<T>(items: &[T])
where
    T: ListItem,
    T::ListFilter: Quantified<Item = T> + Default,
{
    let filter = T::ListFilter::default();
    let cx = MatchCx::at(0);
    round_trip(&filter);

    assert_eq!(filter.is_null(), None, "a default list filter asks nothing");
    let quantifiers = filter.quantifiers();
    assert!(
        quantifiers.some.is_none() && quantifiers.every.is_none() && quantifiers.none.is_none(),
        "a default list filter carries no quantifiers"
    );

    T::list_test(items, &filter, &cx);
    assert!(
        T::list_confirm(items, &filter, &cx).await,
        "a list filter that asks nothing matches every list"
    );
}

/// Run every function the macro wrote for one type's sort keys.
///
/// `fields` is the generated enum's own `ALL`, so a type that gains a sort key
/// gains its measurement with it.
pub(crate) fn exercise_order<T, F>(value: &T, fields: &[F])
where
    F: OrderField,
    T: for<'a> Orderable<MatchCx<'a>, Field = F>,
{
    let cx = MatchCx::at(0);
    for field in fields {
        value.key(*field, &cx);
        assert!(
            !field.wire().is_empty(),
            "every sort key travels under a name"
        );
    }
}
