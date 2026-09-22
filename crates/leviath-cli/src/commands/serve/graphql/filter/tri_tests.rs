//! Three-valued logic, and what the accumulator does with it.

use super::super::scalars::StringFilter;
use super::{Acc, MatchCx, Tri};

/// A filter that only ever matches one string.
fn only(text: &str) -> StringFilter {
    StringFilter {
        eq: Some(text.to_owned()),
        ..StringFilter::default()
    }
}

/// `No` beats everything under `and`, and `Yes` beats everything under `or`.
#[test]
fn a_settled_answer_wins() {
    for other in [Tri::No, Tri::Yes, Tri::Io] {
        assert_eq!(Tri::No.and(other), Tri::No);
        assert_eq!(other.and(Tri::No), Tri::No);
        assert_eq!(Tri::Yes.or(other), Tri::Yes);
        assert_eq!(other.or(Tri::Yes), Tri::Yes);
    }
    assert_eq!(Tri::Yes.and(Tri::Yes), Tri::Yes);
    assert_eq!(Tri::No.or(Tri::No), Tri::No);
    assert_eq!(Tri::Io.and(Tri::Yes), Tri::Io);
    assert_eq!(Tri::Yes.and(Tri::Io), Tri::Io);
    assert_eq!(Tri::Io.or(Tri::No), Tri::Io);
    assert_eq!(Tri::No.or(Tri::Io), Tri::Io);
}

/// Negation turns the settled answers over and leaves the undecided one.
#[test]
fn undecided_stays_undecided_under_negation() {
    assert_eq!(Tri::No.not(), Tri::Yes);
    assert_eq!(Tri::Yes.not(), Tri::No);
    assert_eq!(Tri::Io.not(), Tri::Io);
    assert_eq!(Tri::of(true), Tri::Yes);
    assert_eq!(Tri::of(false), Tri::No);
}

/// Nothing at all satisfies `every`, and nothing at all satisfies no `some`.
#[test]
fn an_empty_run_of_answers_has_a_side() {
    assert_eq!(Tri::all(std::iter::empty()), Tri::Yes);
    assert_eq!(Tri::any(std::iter::empty()), Tri::No);
    assert_eq!(Tri::all([Tri::Yes, Tri::Io].into_iter()), Tri::Io);
    assert_eq!(Tri::any([Tri::No, Tri::Yes].into_iter()), Tri::Yes);
}

/// An accumulator that has seen nothing agrees, whichever way it was made.
#[test]
fn an_empty_accumulator_agrees() {
    assert_eq!(Acc::new().verdict(), Tri::Yes);
    assert_eq!(Acc::default().verdict(), Tri::Yes);
}

/// `isNull: true` refuses a value that is there; `false` and unset do not.
#[test]
fn a_value_that_is_there_is_not_null() {
    for (asked, expected) in [
        (None, Tri::Yes),
        (Some(false), Tri::Yes),
        (Some(true), Tri::No),
    ] {
        let mut acc = Acc::new();
        acc.present(asked);
        assert_eq!(acc.verdict(), expected);
    }
}

/// A field with no filter set on it says nothing either way.
#[test]
fn a_field_nobody_asked_about_says_nothing() {
    let cx = MatchCx::at(0);
    let mut acc = Acc::new();
    acc.field(None::<&StringFilter>, "anything", &cx);
    assert_eq!(acc.verdict(), Tri::Yes);
    acc.field(Some(&only("anything")), "anything", &cx);
    assert_eq!(acc.verdict(), Tri::Yes);
    acc.field(Some(&only("something else")), "anything", &cx);
    assert_eq!(acc.verdict(), Tri::No);
}

/// Once the answer is no, the fields after it are not evaluated at all.
#[test]
fn a_refused_accumulator_stops_asking() {
    let cx = MatchCx::at(0);
    let mut acc = Acc::new();
    acc.field(Some(&only("one")), "two", &cx);
    assert_eq!(acc.verdict(), Tri::No);
    acc.field(Some(&only("two")), "two", &cx);
    assert_eq!(acc.verdict(), Tri::No, "a settled refusal is not revisited");
    acc.present(None);
    assert_eq!(acc.verdict(), Tri::No);
}

/// A field that costs a read is noted rather than answered.
#[test]
fn a_read_is_noted_only_when_it_was_asked_about() {
    let mut acc = Acc::new();
    acc.pending(&None::<StringFilter>);
    assert_eq!(acc.verdict(), Tri::Yes);
    acc.pending(&Some(only("anything")));
    assert_eq!(acc.verdict(), Tri::Io);
}

/// A filter on a variant the value is not never matches.
#[test]
fn a_filter_on_another_variant_refuses() {
    let cx = MatchCx::at(0);
    let mut acc = Acc::new();
    acc.variant(Some(&only("one")), "one", &cx, &[true, false], 0);
    assert_eq!(acc.verdict(), Tri::Yes);

    let mut acc = Acc::new();
    acc.variant(None::<&StringFilter>, "one", &cx, &[false, true], 0);
    assert_eq!(acc.verdict(), Tri::No, "the filter names the other variant");
}
