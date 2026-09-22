//! The contract's own small pieces: the borrowed parts, and the sort keys.

use super::super::scalars::StringFilter;
use super::{Choices, CursorKey, Parts, Quantifiers, sort_key};

/// A filter that only ever matches one string.
fn only(text: &str) -> StringFilter {
    StringFilter {
        eq: Some(text.to_owned()),
        ..StringFilter::default()
    }
}

/// The combinators arrive as they were handed over.
#[test]
fn the_parts_are_what_was_handed_over() {
    let filters = [only("one"), only("two")];
    let parts = Parts::new(Some(&filters), None, Some(&filters[0]), Some(true));
    assert_eq!(parts.and.map(<[StringFilter]>::len), Some(2));
    assert!(parts.or.is_none());
    assert_eq!(parts.not.and_then(|each| each.eq.as_deref()), Some("one"));
    assert_eq!(parts.is_null, Some(true));
    assert!(format!("{parts:?}").starts_with("Parts"));
}

/// The quantifiers arrive as they were handed over.
#[test]
fn the_quantifiers_are_what_was_handed_over() {
    let some = only("one");
    let quantifiers = Quantifiers::new(Some(&some), None, None);
    assert_eq!(
        quantifiers.some.and_then(|each| each.eq.as_deref()),
        Some("one")
    );
    assert!(quantifiers.every.is_none() && quantifiers.none.is_none());
    assert!(format!("{quantifiers:?}").starts_with("Quantifiers"));
}

/// An enum comparator's choices arrive as they were handed over.
#[test]
fn the_choices_are_what_was_handed_over() {
    let set = [1, 2];
    let choices = Choices::new(Some(1), None, Some(&set), None, Some(false));
    assert_eq!(choices.eq, Some(1));
    assert!(choices.ne.is_none());
    assert_eq!(choices.within, Some(&set[..]));
    assert!(choices.not_in.is_none());
    assert_eq!(choices.is_null, Some(false));
    assert!(format!("{choices:?}").starts_with("Choices"));
}

/// A sort key is the value in the form the cursor encodes.
#[test]
fn a_sort_key_is_the_value_the_cursor_encodes() {
    assert_eq!(sort_key("one"), CursorKey::Text("one".to_owned()));
    assert_eq!(sort_key(&12_i32), CursorKey::Int(12));
    assert_eq!(sort_key(&None::<i32>), CursorKey::Null);
    assert_eq!(
        CursorKey::Tuple(vec![CursorKey::Null]),
        CursorKey::Tuple(vec![CursorKey::Null]),
        "a many-key order compares left to right"
    );
    assert!(format!("{:?}", CursorKey::Null).contains("Null"));
}
