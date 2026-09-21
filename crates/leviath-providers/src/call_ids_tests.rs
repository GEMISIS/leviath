//! Tests for the minted tool-call ids.

use super::mint;

/// The label leads, so a reader can tell which provider minted an id, and the
/// rest is what makes it unique.
#[test]
fn an_id_carries_its_label_a_process_prefix_and_a_sequence() {
    let first = mint("probe");
    let second = mint("probe");
    assert!(first.starts_with("probe_"), "{first}");
    assert_ne!(first, second, "two calls are two ids");

    let parts: Vec<&str> = first.split('_').collect();
    assert_eq!(parts.len(), 3, "label, prefix, sequence: {first}");
    assert_eq!(parts[1].len(), 8, "eight hex of process prefix: {first}");
    assert!(
        parts[1].chars().all(|c| c.is_ascii_hexdigit()),
        "the prefix is hex: {first}"
    );

    // One prefix per process, whatever the label, so two providers minting at
    // once cannot land on the same id.
    let other = mint("other");
    assert_eq!(
        other.split('_').nth(1),
        first.split('_').nth(1),
        "one process, one prefix"
    );
    assert_ne!(
        other.split('_').next_back(),
        first.split('_').next_back(),
        "and one sequence across labels"
    );
}
