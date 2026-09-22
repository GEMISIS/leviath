//! Tests for the script vocabularies.
//!
//! The words come from the REST routes' own `{kind}` segments and from the
//! listing's `source`, so what is asserted here is that every word those
//! surfaces can carry has a value, and that one they never carry lands
//! somewhere honest rather than panicking.

use super::{ScriptKind, ScriptScope};
use crate::commands::serve::graphql::filter::testkit::exercise_enum;

/// Every kind word reads back, and the round trip through `wire` holds.
#[test]
fn every_kind_word_reads_back() {
    for (word, kind) in [
        ("tool", ScriptKind::Tool),
        ("region_hook", ScriptKind::RegionHook),
        ("stage_hook", ScriptKind::StageHook),
        ("output_validator", ScriptKind::OutputValidator),
        ("mime_check", ScriptKind::MimeCheck),
        ("provider", ScriptKind::Provider),
        ("unknown", ScriptKind::Candidate),
    ] {
        assert_eq!(ScriptKind::from_wire(word), kind);
        assert_eq!(kind.wire(), word);
    }
}

/// A word no registry claims is a candidate: a file that is there and belongs
/// to nothing yet.
#[test]
fn an_unclaimed_word_is_a_candidate() {
    assert_eq!(
        ScriptKind::from_wire("interpretive_dance"),
        ScriptKind::Candidate
    );
}

/// The two directories a listing walks, and what anything else reads as.
#[test]
fn every_scope_word_reads_back() {
    assert_eq!(ScriptScope::from_wire("agent"), ScriptScope::Blueprint);
    assert_eq!(ScriptScope::from_wire("global"), ScriptScope::Global);
    // A word the listing does not carry: the machine-wide reading, which is the
    // one that claims nothing about a blueprint.
    assert_eq!(
        ScriptScope::from_wire("somewhere else"),
        ScriptScope::Global
    );
}

/// Every function `#[mirror]` wrote for these two enums runs at least once.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[ScriptKind::Tool, ScriptKind::Candidate]).await;
    exercise_enum(&[ScriptScope::Global, ScriptScope::Blueprint]).await;
}
