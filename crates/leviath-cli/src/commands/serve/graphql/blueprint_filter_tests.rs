//! Tests for the blueprint filter.
//!
//! The matcher is exercised against catalogue rows directly, because that is
//! what the listing consults, and the wire shape is exercised through the
//! parser, including the field that is read last.

use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

use super::super::super::types::BlueprintInfo;
use super::super::filters::StringFilter;
use super::BlueprintFilter;

/// One catalogue row, as discovery would have produced it.
fn row(name: &str, version: &str, description: &str) -> BlueprintInfo {
    let manifest = format!(
        "[agent]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"{description}\"\n"
    );
    let parsed = leviath_core::manifest::parse_manifest(&manifest).expect("the manifest parses");
    BlueprintInfo {
        parsed: std::sync::Arc::new(parsed),
        name: name.to_string(),
        version: version.to_string(),
        description: description.to_string(),
        path: format!("/agents/{name}"),
        stages: Vec::new(),
        manifest,
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

/// A filter with nothing set keeps the whole catalogue.
#[test]
fn an_empty_filter_keeps_everything() {
    let matcher = BlueprintFilter::default().compiled();
    assert!(matcher.is_empty());
    assert!(matcher.matches(&row("coder", "1.0.0", "writes code")));
}

/// Each field selects on the value it names, and every field set has to hold.
#[test]
fn each_field_selects_on_its_own_value() {
    let coder = row("coder", "1.0.0", "writes code");
    let writer = row("writer", "2.0.0", "writes prose");

    let by_name = BlueprintFilter {
        name: Some(StringFilter {
            eq: Some("coder".to_string()),
            ..StringFilter::default()
        }),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(by_name.matches(&coder));
    assert!(!by_name.matches(&writer));

    let by_version = BlueprintFilter {
        version: Some(StringFilter {
            starts_with: Some("2.".to_string()),
            ..StringFilter::default()
        }),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(by_version.matches(&writer));
    assert!(!by_version.matches(&coder));

    let by_description = BlueprintFilter {
        description: Some(StringFilter {
            contains: Some("prose".to_string()),
            ..StringFilter::default()
        }),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(by_description.matches(&writer));
    assert!(!by_description.matches(&coder));

    let both = BlueprintFilter {
        name: Some(StringFilter {
            contains: Some("er".to_string()),
            ..StringFilter::default()
        }),
        version: Some(StringFilter {
            eq: Some("1.0.0".to_string()),
            ..StringFilter::default()
        }),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(both.matches(&coder));
    assert!(!both.matches(&writer), "every field set has to hold");
}

/// `query` is the shorthand for a case-insensitive prefix on the name.
#[test]
fn query_is_a_prefix_on_the_name() {
    let matcher = BlueprintFilter {
        query: Some("CO".to_string()),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(matcher.matches(&row("coder", "1.0.0", "")));
    assert!(!matcher.matches(&row("writer", "1.0.0", "")));
}

/// `names` is a membership test, and the same list is what the listing reads
/// directly when the request passes it rather than nesting it.
#[test]
fn names_select_a_set_and_are_readable_before_compiling() {
    let filter = BlueprintFilter {
        names: Some(vec!["coder".to_string(), "ghost".to_string()]),
        ..BlueprintFilter::default()
    };
    assert_eq!(
        filter.exact_names(),
        Some(vec!["coder".to_string(), "ghost".to_string()])
    );
    let matcher = filter.compiled();
    assert!(matcher.matches(&row("coder", "1.0.0", "")));
    assert!(!matcher.matches(&row("writer", "1.0.0", "")));
    assert!(BlueprintFilter::default().exact_names().is_none());
}

/// The combinators compose, including one nested inside another.
#[test]
fn the_combinators_compose_and_nest() {
    let coder = row("coder", "1.0.0", "writes code");
    let writer = row("writer", "2.0.0", "writes prose");
    let auditor = row("auditor", "1.0.0", "reads code");

    let named = |name: &str| BlueprintFilter {
        name: Some(StringFilter {
            eq: Some(name.to_string()),
            ..StringFilter::default()
        }),
        ..BlueprintFilter::default()
    };

    let either = BlueprintFilter {
        or: Some(vec![named("coder"), named("writer")]),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(either.matches(&coder));
    assert!(either.matches(&writer));
    assert!(!either.matches(&auditor));

    let both = BlueprintFilter {
        and: Some(vec![
            BlueprintFilter {
                version: Some(StringFilter {
                    eq: Some("1.0.0".to_string()),
                    ..StringFilter::default()
                }),
                ..BlueprintFilter::default()
            },
            BlueprintFilter {
                description: Some(StringFilter {
                    contains: Some("code".to_string()),
                    ..StringFilter::default()
                }),
                ..BlueprintFilter::default()
            },
        ]),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(both.matches(&coder));
    assert!(both.matches(&auditor));
    assert!(!both.matches(&writer));

    // Version 1.0.0, and not one of the two that write something.
    let nested = BlueprintFilter {
        version: Some(StringFilter {
            eq: Some("1.0.0".to_string()),
            ..StringFilter::default()
        }),
        not: Some(Box::new(BlueprintFilter {
            or: Some(vec![named("coder"), named("writer")]),
            ..BlueprintFilter::default()
        })),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(nested.matches(&auditor));
    assert!(!nested.matches(&coder));
    assert!(!nested.matches(&writer));

    // An alternation with no alternatives selects nothing, which is what an
    // empty `or` means rather than a filter a client can ignore.
    let nothing = BlueprintFilter {
        or: Some(Vec::new()),
        ..BlueprintFilter::default()
    }
    .compiled();
    assert!(!nothing.matches(&coder));
    assert!(!nothing.is_empty());
}

/// Two different filters digest differently, and the same filter digests the
/// same way twice, which is what a cursor's promise rests on.
#[test]
fn the_digest_follows_the_filter() {
    let of = |name: &str| {
        BlueprintFilter {
            name: Some(StringFilter {
                eq: Some(name.to_string()),
                ..StringFilter::default()
            }),
            ..BlueprintFilter::default()
        }
        .compiled()
        .digest_part()
    };
    assert_eq!(of("coder"), of("coder"));
    assert_ne!(of("coder"), of("writer"));
}

/// A value of the wrong type is refused wherever it sits, first field or last.
#[test]
fn the_blueprint_filter_refuses_what_it_cannot_read() {
    let wrong = Value::Number(7.into());
    assert!(BlueprintFilter::parse(Some(Value::String("nope".into()))).is_err());
    assert!(BlueprintFilter::parse(Some(object(&[("query", wrong.clone())]))).is_err());
    assert!(
        BlueprintFilter::parse(Some(object(&[
            ("query", Value::String("co".into())),
            ("names", wrong.clone()),
        ])))
        .is_err()
    );
    assert!(
        BlueprintFilter::parse(Some(object(&[
            ("query", Value::String("co".into())),
            ("description", wrong),
        ])))
        .is_err(),
        "the last field is read after every other one"
    );
}
