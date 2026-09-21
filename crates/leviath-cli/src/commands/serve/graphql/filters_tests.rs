//! Tests for the scalar filters.
//!
//! Two things are checked for each one. That every comparison it offers means
//! what it says, both ways round, because a filter that quietly ignores a
//! field is worse than one that refuses it. And that a value of the wrong type
//! is refused wherever it sits in the object, including the last field: the
//! fields are read in turn, and a check that only ever sends a bad first field
//! proves nothing about the rest.

use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

use super::super::scalars::{Decimal, Timestamp};
use super::{BooleanFilter, DecimalFilter, IntFilter, Ordered, StringFilter, TimestampFilter};

/// An ordered filter carrying no comparison at all.
fn unbounded<T>() -> Ordered<T> {
    Ordered {
        eq: None,
        ne: None,
        within: None,
        outside: None,
        lt: None,
        lte: None,
        gt: None,
        gte: None,
    }
}

/// One object value, built from its fields in the order they are given.
fn object(fields: &[(&str, Value)]) -> Value {
    let mut map = IndexMap::new();
    for (name, value) in fields {
        map.insert(Name::new(*name), value.clone());
    }
    Value::Object(map)
}

// ─── the ordered matcher ────────────────────────────────────────────────────

/// Each comparison keeps what it names and drops what it does not.
#[test]
fn every_ordered_comparison_means_what_it_says() {
    let cases: [(Ordered<i64>, i64, i64); 8] = [
        (
            Ordered {
                eq: Some(10),
                ..unbounded()
            },
            10,
            11,
        ),
        (
            Ordered {
                ne: Some(10),
                ..unbounded()
            },
            11,
            10,
        ),
        (
            Ordered {
                within: Some(vec![1, 10]),
                ..unbounded()
            },
            10,
            5,
        ),
        (
            Ordered {
                outside: Some(vec![1, 10]),
                ..unbounded()
            },
            5,
            10,
        ),
        (
            Ordered {
                lt: Some(10),
                ..unbounded()
            },
            9,
            10,
        ),
        (
            Ordered {
                lte: Some(10),
                ..unbounded()
            },
            10,
            11,
        ),
        (
            Ordered {
                gt: Some(10),
                ..unbounded()
            },
            11,
            10,
        ),
        (
            Ordered {
                gte: Some(10),
                ..unbounded()
            },
            10,
            9,
        ),
    ];
    for (index, (filter, kept, dropped)) in cases.into_iter().enumerate() {
        assert!(filter.matches(&kept), "case {index} keeps {kept}");
        assert!(!filter.matches(&dropped), "case {index} drops {dropped}");
    }
}

/// A filter with nothing set keeps everything, and two bounds together are a
/// range rather than an alternative.
#[test]
fn ordered_bounds_compose_into_a_range() {
    let anything: Ordered<i64> = unbounded();
    assert!(anything.matches(&0));
    let window = Ordered {
        gte: Some(10),
        lt: Some(20),
        ..unbounded()
    };
    assert!(window.matches(&10));
    assert!(window.matches(&19));
    assert!(!window.matches(&9));
    assert!(!window.matches(&20));
}

/// A field the run has no value for satisfies no comparison, whatever the
/// comparison is over.
#[test]
fn an_absent_value_satisfies_no_ordered_comparison() {
    let money = Ordered {
        gt: Some(Decimal(0.0)),
        ..unbounded()
    };
    assert!(money.matches_option(Some(Decimal(0.25))));
    assert!(!money.matches_option(None));
    let seconds: Ordered<i64> = Ordered {
        lt: Some(60),
        ..unbounded()
    };
    assert!(seconds.matches_option(Some(30)));
    assert!(!seconds.matches_option(None));
}

/// Spend compares as an amount rather than as text, so "under a dollar" is
/// one bound and not a string prefix.
#[test]
fn spend_compares_as_an_amount() {
    let under_a_dollar = DecimalFilter {
        lt: Some(Decimal(1.0)),
        ..DecimalFilter::default()
    }
    .compiled();
    assert!(under_a_dollar.matches(&Decimal(0.9)));
    assert!(!under_a_dollar.matches(&Decimal(10.0)));
}

// ─── the text matcher ───────────────────────────────────────────────────────

/// Each text comparison keeps what it names and drops what it does not.
#[test]
fn every_text_comparison_means_what_it_says() {
    let cases = [
        (
            StringFilter {
                eq: Some("coder".to_string()),
                ..StringFilter::default()
            },
            "coder",
            "Coder",
        ),
        (
            StringFilter {
                ne: Some("coder".to_string()),
                ..StringFilter::default()
            },
            "writer",
            "coder",
        ),
        (
            StringFilter {
                within: Some(vec!["coder".to_string(), "writer".to_string()]),
                ..StringFilter::default()
            },
            "writer",
            "auditor",
        ),
        (
            StringFilter {
                not_in: Some(vec!["coder".to_string()]),
                ..StringFilter::default()
            },
            "writer",
            "coder",
        ),
        (
            StringFilter {
                contains: Some("OD".to_string()),
                ..StringFilter::default()
            },
            "coder",
            "writer",
        ),
        (
            StringFilter {
                starts_with: Some("CO".to_string()),
                ..StringFilter::default()
            },
            "coder",
            "recoder",
        ),
        (
            StringFilter {
                ends_with: Some("ER".to_string()),
                ..StringFilter::default()
            },
            "coder",
            "coding",
        ),
    ];
    for (index, (filter, kept, dropped)) in cases.into_iter().enumerate() {
        let text = filter.compiled();
        assert!(text.matches(kept), "case {index} keeps {kept}");
        assert!(!text.matches(dropped), "case {index} drops {dropped}");
    }
}

/// A text filter with nothing set keeps everything, and a run with no value
/// for the field satisfies nothing.
#[test]
fn absent_text_satisfies_nothing_and_an_empty_filter_keeps_all() {
    let anything = StringFilter::default().compiled();
    assert!(anything.matches("whatever"));
    assert!(anything.matches_option(Some("whatever")));
    assert!(
        !anything.matches_option(None),
        "there is no value to compare"
    );
    let titled = StringFilter {
        contains: Some("ship".to_string()),
        ..StringFilter::default()
    }
    .compiled();
    assert!(titled.matches_option(Some("ship the release")));
    assert!(!titled.matches_option(None));
}

// ─── the boolean matcher ────────────────────────────────────────────────────

/// `eq` and `ne` are the two halves of one question, and an empty filter keeps
/// both answers.
#[test]
fn a_boolean_filter_reads_both_ways() {
    let unattended = BooleanFilter {
        eq: Some(true),
        ..BooleanFilter::default()
    }
    .compiled();
    assert!(unattended.matches(true));
    assert!(!unattended.matches(false));

    let attended = BooleanFilter {
        ne: Some(true),
        ..BooleanFilter::default()
    }
    .compiled();
    assert!(attended.matches(false));
    assert!(!attended.matches(true));

    assert!(BooleanFilter::default().compiled().matches(true));
    assert!(BooleanFilter::default().compiled().matches(false));
}

// ─── compiling ──────────────────────────────────────────────────────────────

/// A whole-number filter widens to the 64 bits the values are read as, every
/// field included, so a bound never compares at the edge of a 32-bit number.
#[test]
fn a_whole_number_filter_widens_every_bound() {
    let compiled = IntFilter {
        eq: Some(1),
        ne: Some(2),
        within: Some(vec![3, 4]),
        not_in: Some(vec![5]),
        lt: Some(6),
        lte: Some(7),
        gt: Some(8),
        gte: Some(9),
    }
    .compiled();
    assert_eq!(
        compiled,
        Ordered {
            eq: Some(1i64),
            ne: Some(2i64),
            within: Some(vec![3i64, 4i64]),
            outside: Some(vec![5i64]),
            lt: Some(6i64),
            lte: Some(7i64),
            gt: Some(8i64),
            gte: Some(9i64),
        }
    );
}

/// A timestamp filter compiles to the seconds a run records, every field
/// included.
#[test]
fn a_timestamp_filter_compiles_to_seconds() {
    let compiled = TimestampFilter {
        eq: Some(Timestamp(1)),
        ne: Some(Timestamp(2)),
        within: Some(vec![Timestamp(3), Timestamp(4)]),
        not_in: Some(vec![Timestamp(5)]),
        lt: Some(Timestamp(6)),
        lte: Some(Timestamp(7)),
        gt: Some(Timestamp(8)),
        gte: Some(Timestamp(9)),
    }
    .compiled();
    assert_eq!(
        compiled,
        Ordered {
            eq: Some(1i64),
            ne: Some(2i64),
            within: Some(vec![3i64, 4i64]),
            outside: Some(vec![5i64]),
            lt: Some(6i64),
            lte: Some(7i64),
            gt: Some(8i64),
            gte: Some(9i64),
        }
    );
}

/// An exact-decimal filter carries every bound through as an amount.
#[test]
fn a_decimal_filter_compiles_every_bound() {
    let compiled = DecimalFilter {
        eq: Some(Decimal(1.0)),
        ne: Some(Decimal(2.0)),
        within: Some(vec![Decimal(3.0)]),
        not_in: Some(vec![Decimal(4.0)]),
        lt: Some(Decimal(5.0)),
        lte: Some(Decimal(6.0)),
        gt: Some(Decimal(7.0)),
        gte: Some(Decimal(8.0)),
    }
    .compiled();
    assert_eq!(
        compiled,
        Ordered {
            eq: Some(Decimal(1.0)),
            ne: Some(Decimal(2.0)),
            within: Some(vec![Decimal(3.0)]),
            outside: Some(vec![Decimal(4.0)]),
            lt: Some(Decimal(5.0)),
            lte: Some(Decimal(6.0)),
            gt: Some(Decimal(7.0)),
            gte: Some(Decimal(8.0)),
        }
    );
}

/// Text compiles field for field, `notIn` landing where the matcher calls it.
#[test]
fn a_string_filter_compiles_every_field() {
    let compiled = StringFilter {
        eq: Some("a".to_string()),
        ne: Some("b".to_string()),
        within: Some(vec!["c".to_string()]),
        not_in: Some(vec!["d".to_string()]),
        contains: Some("e".to_string()),
        starts_with: Some("f".to_string()),
        ends_with: Some("g".to_string()),
    }
    .compiled();
    assert_eq!(compiled.eq.as_deref(), Some("a"));
    assert_eq!(compiled.ne.as_deref(), Some("b"));
    assert_eq!(compiled.within, Some(vec!["c".to_string()]));
    assert_eq!(compiled.outside, Some(vec!["d".to_string()]));
    assert_eq!(compiled.contains.as_deref(), Some("e"));
    assert_eq!(compiled.starts_with.as_deref(), Some("f"));
    assert_eq!(compiled.ends_with.as_deref(), Some("g"));
}

// ─── reading one off the wire ───────────────────────────────────────────────

/// A value of the wrong type is refused wherever it sits, first field or last.
///
/// The fields of an input object are read in turn, so a check that only ever
/// sends a bad first field says nothing about what the later ones do.
#[test]
fn a_scalar_filter_refuses_what_it_cannot_read() {
    let wrong = Value::Number(7.into());
    assert!(StringFilter::parse(Some(Value::String("nope".into()))).is_err());
    assert!(StringFilter::parse(Some(object(&[("eq", wrong.clone())]))).is_err());
    assert!(
        StringFilter::parse(Some(object(&[
            ("eq", Value::String("coder".into())),
            ("endsWith", wrong.clone()),
        ])))
        .is_err(),
        "the last field is read after every other one"
    );

    assert!(IntFilter::parse(Some(object(&[("eq", Value::String("x".into()))]))).is_err());
    assert!(
        IntFilter::parse(Some(object(&[
            ("eq", Value::Number(1.into())),
            ("gte", Value::String("x".into())),
        ])))
        .is_err()
    );

    assert!(TimestampFilter::parse(Some(object(&[("eq", Value::String("x".into()))]))).is_err());
    assert!(
        TimestampFilter::parse(Some(object(&[
            ("eq", Value::Number(1.into())),
            ("gte", Value::String("x".into())),
        ])))
        .is_err()
    );

    // A decimal travels as a string, so a bare number is the wrong type here.
    assert!(DecimalFilter::parse(Some(object(&[("eq", wrong.clone())]))).is_err());
    assert!(
        DecimalFilter::parse(Some(object(&[
            ("eq", Value::String("0.25".into())),
            ("gte", wrong.clone()),
        ])))
        .is_err()
    );

    assert!(BooleanFilter::parse(Some(object(&[("eq", wrong.clone())]))).is_err());
    assert!(
        BooleanFilter::parse(Some(object(
            &[("eq", Value::Boolean(true)), ("ne", wrong),]
        )))
        .is_err()
    );
}

/// Every field reads back off the wire, so none of them is a field only the
/// schema knows about.
#[test]
fn a_scalar_filter_reads_every_field_it_declares() {
    let text = StringFilter::parse(Some(object(&[
        ("eq", Value::String("a".into())),
        ("ne", Value::String("b".into())),
        ("in", Value::List(vec![Value::String("c".into())])),
        ("notIn", Value::List(vec![Value::String("d".into())])),
        ("contains", Value::String("e".into())),
        ("startsWith", Value::String("f".into())),
        ("endsWith", Value::String("g".into())),
    ])))
    .expect("every field reads");
    assert_eq!(text.within, Some(vec!["c".to_string()]));
    assert_eq!(text.not_in, Some(vec!["d".to_string()]));

    let numbers = IntFilter::parse(Some(object(&[
        ("in", Value::List(vec![Value::Number(1.into())])),
        ("notIn", Value::List(vec![Value::Number(2.into())])),
        ("lt", Value::Number(3.into())),
        ("lte", Value::Number(4.into())),
        ("gt", Value::Number(5.into())),
        ("gte", Value::Number(6.into())),
    ])))
    .expect("every field reads");
    assert_eq!(numbers.within, Some(vec![1]));
    assert_eq!(numbers.not_in, Some(vec![2]));

    let money = DecimalFilter::parse(Some(object(&[
        ("in", Value::List(vec![Value::String("0.25".into())])),
        ("notIn", Value::List(vec![Value::String("0.50".into())])),
    ])))
    .expect("every field reads");
    assert_eq!(money.within, Some(vec![Decimal(0.25)]));
    assert_eq!(money.not_in, Some(vec![Decimal(0.5)]));

    let times = TimestampFilter::parse(Some(object(&[
        ("in", Value::List(vec![Value::Number(7.into())])),
        ("notIn", Value::List(vec![Value::Number(8.into())])),
    ])))
    .expect("every field reads");
    assert_eq!(times.within, Some(vec![Timestamp(7)]));
    assert_eq!(times.not_in, Some(vec![Timestamp(8)]));
}
