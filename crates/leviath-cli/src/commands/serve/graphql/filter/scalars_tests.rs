//! The shared wire filters, one comparison at a time.

use async_graphql::{
    EmptyMutation, EmptySubscription, ID, InputType, Name, Object, Schema, Value,
    indexmap::IndexMap,
};

use super::{
    BigInt, BigIntFilter, BooleanFilter, Bound, Decimal, DecimalFilter, FloatFilter, IDFilter,
    IntFilter, JSONFilter, Json, Nullable, StringFilter, StringListFilter, Timestamp,
    TimestampFilter, exact_test, ordered_test, string_list_test, text_test,
};
use crate::commands::serve::graphql::filter::testkit::round_trip;

/// A whole-number filter with nothing set.
fn ints() -> IntFilter {
    IntFilter::default()
}

/// Exact comparisons are exact, and case-sensitive.
#[test]
fn exact_text_is_exact() {
    let value = "Plan the work";
    assert!(text_test(value, &StringFilter::default()));
    assert!(text_test(
        value,
        &StringFilter {
            eq: Some(value.to_owned()),
            ..StringFilter::default()
        }
    ));
    assert!(!text_test(
        value,
        &StringFilter {
            eq: Some("plan the work".to_owned()),
            ..StringFilter::default()
        }
    ));
    assert!(text_test(
        value,
        &StringFilter {
            ne: Some("something".to_owned()),
            ..StringFilter::default()
        }
    ));
    assert!(!text_test(
        value,
        &StringFilter {
            ne: Some(value.to_owned()),
            ..StringFilter::default()
        }
    ));
}

/// Set membership is exact too, both ways round.
#[test]
fn text_sets_are_exact() {
    let value = "plan";
    let within = |set: &[&str]| StringFilter {
        within: Some(set.iter().map(|each| (*each).to_owned()).collect()),
        ..StringFilter::default()
    };
    assert!(text_test(value, &within(&["plan", "write"])));
    assert!(!text_test(value, &within(&["Plan"])));

    let not_in = |set: &[&str]| StringFilter {
        not_in: Some(set.iter().map(|each| (*each).to_owned()).collect()),
        ..StringFilter::default()
    };
    assert!(text_test(value, &not_in(&["write"])));
    assert!(!text_test(value, &not_in(&["plan"])));
}

/// Substring comparisons ignore ASCII case, as the run search does.
#[test]
fn substrings_ignore_ascii_case() {
    let value = "Plan the work";
    let holds = |filter: StringFilter| text_test(value, &filter);
    assert!(holds(StringFilter {
        contains: Some("THE".to_owned()),
        ..StringFilter::default()
    }));
    assert!(!holds(StringFilter {
        contains: Some("nothing".to_owned()),
        ..StringFilter::default()
    }));
    assert!(holds(StringFilter {
        starts_with: Some("plan".to_owned()),
        ..StringFilter::default()
    }));
    assert!(!holds(StringFilter {
        starts_with: Some("work".to_owned()),
        ..StringFilter::default()
    }));
    assert!(holds(StringFilter {
        ends_with: Some("WORK".to_owned()),
        ..StringFilter::default()
    }));
    assert!(!holds(StringFilter {
        ends_with: Some("plan".to_owned()),
        ..StringFilter::default()
    }));
}

/// A value that is here never matches "there is no value".
#[test]
fn a_value_that_is_here_is_not_null() {
    assert!(!text_test(
        "plan",
        &StringFilter {
            is_null: Some(true),
            ..StringFilter::default()
        }
    ));
    assert!(text_test(
        "plan",
        &StringFilter {
            is_null: Some(false),
            ..StringFilter::default()
        }
    ));
}

/// Every range bound is half-open the way it reads.
#[test]
fn the_range_bounds_read_as_they_are_written() {
    let bound = |filter: IntFilter| ordered_test(&5_i32, &filter.parts());
    assert!(bound(ints()));
    assert!(bound(IntFilter {
        lt: Some(6),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        lt: Some(5),
        ..ints()
    }));
    assert!(bound(IntFilter {
        lte: Some(5),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        lte: Some(4),
        ..ints()
    }));
    assert!(bound(IntFilter {
        gt: Some(4),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        gt: Some(5),
        ..ints()
    }));
    assert!(bound(IntFilter {
        gte: Some(5),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        gte: Some(6),
        ..ints()
    }));
}

/// The exact comparisons work the same way on a number.
#[test]
fn numbers_compare_exactly_too() {
    let bound = |filter: IntFilter| ordered_test(&5_i32, &filter.parts());
    assert!(bound(IntFilter {
        eq: Some(5),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        eq: Some(6),
        ..ints()
    }));
    assert!(bound(IntFilter {
        ne: Some(6),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        ne: Some(5),
        ..ints()
    }));
    assert!(bound(IntFilter {
        within: Some(vec![4, 5]),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        within: Some(vec![4]),
        ..ints()
    }));
    assert!(bound(IntFilter {
        not_in: Some(vec![4]),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        not_in: Some(vec![5]),
        ..ints()
    }));
    assert!(!bound(IntFilter {
        is_null: Some(true),
        ..ints()
    }));
}

/// An id is compared whole, and in a set.
#[test]
fn an_id_is_compared_whole() {
    let value = ID::from("run:one");
    let filter = IDFilter {
        within: Some(vec![ID::from("run:one"), ID::from("run:two")]),
        ..IDFilter::default()
    };
    assert!(exact_test(&value, &filter.parts()));
    assert!(exact_test(
        &value,
        &IDFilter {
            eq: Some(ID::from("run:one")),
            ..IDFilter::default()
        }
        .parts()
    ));
    assert!(!exact_test(
        &value,
        &IDFilter {
            eq: Some(ID::from("run:other")),
            ..IDFilter::default()
        }
        .parts()
    ));
}

/// A 64-bit bound compares against both the wire type and a plain number.
#[test]
fn a_big_number_compares_either_way() {
    let filter = BigIntFilter {
        gte: Some(BigInt(2_147_483_648)),
        ..BigIntFilter::default()
    };
    assert!(ordered_test(&BigInt(2_147_483_649), &filter.parts()));
    assert!(ordered_test(&2_147_483_649_i64, &filter.parts()));
    assert!(!ordered_test(&2_i64, &filter.parts()));
}

/// Money, times, floats and booleans each compare in their own type.
#[test]
fn the_other_scalars_compare_in_their_own_type() {
    assert!(ordered_test(
        &Decimal(0.25),
        &DecimalFilter {
            lte: Some(Decimal(0.25)),
            ..DecimalFilter::default()
        }
        .parts()
    ));
    assert!(ordered_test(
        &Timestamp(100),
        &TimestampFilter {
            gt: Some(Timestamp(99)),
            ..TimestampFilter::default()
        }
        .parts()
    ));
    assert!(ordered_test(
        &0.5_f64,
        &FloatFilter {
            lt: Some(1.0),
            ..FloatFilter::default()
        }
        .parts()
    ));
    assert!(exact_test(
        &true,
        &BooleanFilter {
            eq: Some(true),
            ..BooleanFilter::default()
        }
        .parts()
    ));
    assert!(!exact_test(
        &true,
        &BooleanFilter {
            ne: Some(true),
            ..BooleanFilter::default()
        }
        .parts()
    ));
}

/// JSON is compared whole, because nothing inside it is this schema's to name.
#[test]
fn json_is_compared_whole() {
    let value = Json(serde_json::json!({"depth": 2}));
    assert!(exact_test(
        &value,
        &JSONFilter {
            eq: Some(Json(serde_json::json!({"depth": 2}))),
            ..JSONFilter::default()
        }
        .parts()
    ));
    assert!(!exact_test(
        &value,
        &JSONFilter {
            eq: Some(Json(serde_json::json!({"depth": 3}))),
            ..JSONFilter::default()
        }
        .parts()
    ));
    assert!(exact_test(
        &value,
        &JSONFilter {
            ne: Some(Json(serde_json::json!(null))),
            ..JSONFilter::default()
        }
        .parts()
    ));
}

/// A list of strings is asked about membership.
#[test]
fn a_list_of_strings_is_asked_about_membership() {
    let items = ["one".to_owned(), "two".to_owned()];
    let holds = |filter: StringListFilter| string_list_test(&items, &filter);
    assert!(holds(StringListFilter::default()));
    assert!(holds(StringListFilter {
        has: Some("one".to_owned()),
        ..StringListFilter::default()
    }));
    assert!(!holds(StringListFilter {
        has: Some("three".to_owned()),
        ..StringListFilter::default()
    }));
    assert!(holds(StringListFilter {
        has_every: Some(vec!["one".to_owned(), "two".to_owned()]),
        ..StringListFilter::default()
    }));
    assert!(!holds(StringListFilter {
        has_every: Some(vec!["one".to_owned(), "three".to_owned()]),
        ..StringListFilter::default()
    }));
    assert!(holds(StringListFilter {
        has_some: Some(vec!["three".to_owned(), "two".to_owned()]),
        ..StringListFilter::default()
    }));
    assert!(!holds(StringListFilter {
        has_some: Some(vec!["three".to_owned()]),
        ..StringListFilter::default()
    }));
    assert!(!holds(StringListFilter {
        is_empty: Some(true),
        ..StringListFilter::default()
    }));
    assert!(holds(StringListFilter {
        is_empty: Some(false),
        ..StringListFilter::default()
    }));
    assert!(!holds(StringListFilter {
        is_null: Some(true),
        ..StringListFilter::default()
    }));
    assert!(string_list_test(
        &[] as &[String],
        &StringListFilter {
            is_empty: Some(true),
            ..StringListFilter::default()
        }
    ));
}

/// Every wire filter answers what was asked about an absent value.
#[test]
fn every_filter_answers_about_absence() {
    assert_eq!(
        StringFilter {
            is_null: Some(true),
            ..StringFilter::default()
        }
        .is_null(),
        Some(true)
    );
    assert_eq!(IDFilter::default().is_null(), None);
    assert_eq!(IntFilter::default().is_null(), None);
    assert_eq!(BigIntFilter::default().is_null(), None);
    assert_eq!(FloatFilter::default().is_null(), None);
    assert_eq!(DecimalFilter::default().is_null(), None);
    assert_eq!(TimestampFilter::default().is_null(), None);
    assert_eq!(BooleanFilter::default().is_null(), None);
    assert_eq!(JSONFilter::default().is_null(), None);
    assert_eq!(StringListFilter::default().is_null(), None);
}

/// The borrowed comparisons print as themselves.
#[test]
fn the_borrowed_comparisons_print_as_themselves() {
    assert!(format!("{:?}", ints().parts()).starts_with("Ordered"));
}

/// One object value, built from its fields in the order they are given.
fn object(fields: &[(&str, Value)]) -> Value {
    let mut map = IndexMap::new();
    for (name, value) in fields {
        map.insert(Name::new(*name), value.clone());
    }
    Value::Object(map)
}

/// Every wire filter arrives off the wire, and goes back out the same shape.
///
/// The comparisons above are asked in Rust, which says nothing about whether a
/// client can write one. This reads each filter the way a request does.
#[test]
fn every_wire_filter_is_read_from_the_wire() {
    let text = StringFilter::parse(Some(object(&[
        ("eq", Value::String("plan".into())),
        ("contains", Value::String("la".into())),
        ("isNull", Value::Boolean(false)),
    ])))
    .expect("a text filter");
    assert_eq!(text.eq.as_deref(), Some("plan"));
    assert_eq!(text.is_null, Some(false));
    assert!(matches!(text.to_value(), Value::Object(_)));

    let ids = IDFilter::parse(Some(object(&[("eq", Value::String("run:one".into()))])))
        .expect("an id filter");
    assert_eq!(ids.eq, Some(ID::from("run:one")));
    assert!(matches!(ids.to_value(), Value::Object(_)));

    let numbers =
        IntFilter::parse(Some(object(&[("gte", Value::Number(4.into()))]))).expect("an int filter");
    assert_eq!(numbers.gte, Some(4));
    assert!(matches!(numbers.to_value(), Value::Object(_)));

    let big = BigIntFilter::parse(Some(object(&[("lt", Value::Number(9.into()))])))
        .expect("a big int filter");
    assert_eq!(big.lt, Some(BigInt(9)));
    assert!(matches!(big.to_value(), Value::Object(_)));

    let floats = FloatFilter::parse(Some(object(&[(
        "lte",
        Value::Number(serde_json::Number::from_f64(0.5).expect("finite")),
    )])))
    .expect("a float filter");
    assert_eq!(floats.lte, Some(0.5));
    assert!(matches!(floats.to_value(), Value::Object(_)));

    let money = DecimalFilter::parse(Some(object(&[("gt", Value::String("0.25".into()))])))
        .expect("a decimal filter");
    assert_eq!(money.gt, Some(Decimal(0.25)));
    assert!(matches!(money.to_value(), Value::Object(_)));

    let times = TimestampFilter::parse(Some(object(&[("lt", Value::Number(100.into()))])))
        .expect("a timestamp filter");
    assert_eq!(times.lt, Some(Timestamp(100)));
    assert!(matches!(times.to_value(), Value::Object(_)));

    let flags = BooleanFilter::parse(Some(object(&[("eq", Value::Boolean(true))])))
        .expect("a boolean filter");
    assert_eq!(flags.eq, Some(true));
    assert!(matches!(flags.to_value(), Value::Object(_)));

    let blobs =
        JSONFilter::parse(Some(object(&[("eq", Value::Number(1.into()))]))).expect("a json filter");
    assert_eq!(blobs.eq, Some(Json(serde_json::json!(1))));
    assert!(matches!(blobs.to_value(), Value::Object(_)));

    let lists = StringListFilter::parse(Some(object(&[("has", Value::String("one".into()))])))
        .expect("a list filter");
    assert_eq!(lists.has.as_deref(), Some("one"));
    assert!(matches!(lists.to_value(), Value::Object(_)));
}

/// A value of the wrong type is refused wherever in the object it sits.
///
/// Wherever is the point. The derive reads an input object's fields in
/// declaration order and gives each one its own error path, so a mistake in
/// the first field is refused before any of the others are read. The shared
/// driver walks every field of every filter, which is what reaches the paths a
/// first-field mistake never gets to.
#[test]
fn a_value_of_the_wrong_type_is_refused() {
    round_trip(&StringFilter::default());
    round_trip(&IDFilter::default());
    round_trip(&IntFilter::default());
    round_trip(&BigIntFilter::default());
    round_trip(&FloatFilter::default());
    round_trip(&DecimalFilter::default());
    round_trip(&TimestampFilter::default());
    round_trip(&BooleanFilter::default());
    round_trip(&JSONFilter::default());
    round_trip(&StringListFilter::default());

    // And the shapes that are wrong before any field is read at all.
    let wrong = Value::Boolean(true);
    assert!(StringFilter::parse(Some(Value::String("nope".into()))).is_err());
    assert!(StringFilter::parse(Some(object(&[("eq", wrong.clone())]))).is_err());
    assert!(
        StringFilter::parse(Some(object(&[
            ("eq", Value::String("plan".into())),
            ("isNull", Value::String("maybe".into())),
        ])))
        .is_err(),
        "a mistake in the last field is reached once the first one has read"
    );
    assert!(IDFilter::parse(Some(object(&[("eq", wrong)]))).is_err());
}

/// The root every wire filter is registered through.
struct Query;

#[Object]
impl Query {
    /// Whether every filter on a value arrived.
    async fn asked(
        &self,
        text: Option<StringFilter>,
        id: Option<IDFilter>,
        number: Option<IntFilter>,
        big: Option<BigIntFilter>,
        real: Option<FloatFilter>,
    ) -> bool {
        text.is_some() && id.is_some() && number.is_some() && big.is_some() && real.is_some()
    }

    /// Whether every filter on the rest of them arrived.
    async fn also_asked(
        &self,
        money: Option<DecimalFilter>,
        at: Option<TimestampFilter>,
        flag: Option<BooleanFilter>,
        blob: Option<JSONFilter>,
        list: Option<StringListFilter>,
    ) -> bool {
        money.is_some() && at.is_some() && flag.is_some() && blob.is_some() && list.is_some()
    }
}

/// Every wire filter is a type a client can write, in a real request.
///
/// Reading one in Rust says nothing about whether async-graphql can register
/// it, name it, or read it off a request. This asks all three at once, and it
/// is what caught `IDFilter` registering itself as `Idfilter`.
#[tokio::test]
async fn every_wire_filter_is_a_type_a_client_can_write() {
    let schema = Schema::new(Query, EmptyMutation, EmptySubscription);
    let sdl = schema.sdl();
    for name in [
        "input StringFilter",
        "input IDFilter",
        "input IntFilter",
        "input BigIntFilter",
        "input FloatFilter",
        "input DecimalFilter",
        "input TimestampFilter",
        "input BooleanFilter",
        "input JSONFilter",
        "input StringListFilter",
    ] {
        assert!(sdl.contains(name), "{name} is missing from\n{sdl}");
    }

    let answer = schema
        .execute(
            r#"{
                asked(
                    text: { eq: "plan", contains: "la" }
                    id: { in: ["run:one"] }
                    number: { gte: 1, lt: 9 }
                    big: { lte: 9 }
                    real: { gt: 0.5 }
                )
                alsoAsked(
                    money: { eq: "0.25" }
                    at: { lt: 100 }
                    flag: { ne: false }
                    blob: { eq: 1 }
                    list: { has: "one", isEmpty: false }
                )
            }"#,
        )
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    assert_eq!(
        answer.data.to_string(),
        "{asked: true, alsoAsked: true}",
        "every filter reached the resolver"
    );
}

/// An `Int` bound compares against a 64-bit value.
///
/// A count that travels as `BigInt` is still filtered with the `Int` a client
/// writes, so the bound is widened rather than the value narrowed: a filter
/// cannot silently stop matching because the number outgrew 32 bits.
#[test]
fn an_int_bound_compares_against_a_wider_value() {
    use std::cmp::Ordering;

    assert_eq!(Bound::<i64>::compare(&5i32, &7i64), Some(Ordering::Greater));
    assert_eq!(Bound::<i64>::compare(&5i32, &5i64), Some(Ordering::Equal));
    assert_eq!(Bound::<i64>::compare(&5i32, &1i64), Some(Ordering::Less));
    let past_an_int = i64::from(i32::MAX) + 1;
    assert_eq!(
        Bound::<i64>::compare(&1i32, &past_an_int),
        Some(Ordering::Greater),
        "a value past what an Int holds still compares"
    );
}
