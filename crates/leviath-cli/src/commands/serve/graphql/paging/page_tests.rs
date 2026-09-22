//! Tests for the shared page-size check.

use super::page;

/// The refusal text, for a test that cares what a client is told.
fn refusal(first: i32, cap: usize) -> String {
    match page(first, cap, "the executions page cap") {
        Ok(n) => panic!("{n} was accepted"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_size_within_the_cap_is_taken_as_asked() {
    assert_eq!(page(1, 200, "the cap").expect("accepted"), 1);
    assert_eq!(page(50, 200, "the cap").expect("accepted"), 50);
    assert_eq!(page(200, 200, "the cap").expect("accepted"), 200);
}

/// Zero and a negative are the same mistake, and the message says what to do
/// instead rather than restating the rule.
#[test]
fn a_page_of_nothing_is_refused() {
    let none = refusal(0, 200);
    assert_eq!(none, "`first` must be at least 1; omit it for the default");
    assert_eq!(refusal(-7, 200), none);
}

/// Over the cap is a refusal, not a clamp, and the refusal names which cap.
#[test]
fn a_size_over_the_cap_names_the_cap_it_broke() {
    assert_eq!(
        refusal(201, 200),
        "`first` may be at most 200, the executions page cap"
    );
}

/// A refusal is the request's own fault, so it carries the code a client
/// branches on rather than a server failure.
#[test]
fn a_refusal_is_a_bad_request() {
    let failure = page(0, 200, "the cap").expect_err("refused");
    assert_eq!(failure.code(), "BAD_USER_INPUT");
    assert_eq!(failure.status().as_u16(), 400);
}
