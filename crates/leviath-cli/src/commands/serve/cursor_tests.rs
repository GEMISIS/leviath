//! Tests for the pagination cursor codec.

use super::*;

const DIGEST: &str = "abcd1234";

fn int_cursor() -> String {
    encode(
        "started_at",
        "desc",
        DIGEST,
        CursorKey::Int(1_700_000_000),
        "run-a",
    )
}

#[test]
fn a_cursor_round_trips_through_hex_and_json() {
    let raw = int_cursor();
    let back = decode(&raw, "started_at", "desc", DIGEST).expect("round trip");
    assert_eq!(back.key, CursorKey::Int(1_700_000_000));
    assert_eq!(back.id, "run-a");
    assert_eq!(back.sort, "started_at");
    assert_eq!(back.order, "desc");
    assert_eq!(back.v, CURSOR_VERSION);
}

/// The blueprint catalog sorts by name, so the text variant has to survive the
/// trip as text - this is what `#[serde(untagged)]` would have got wrong.
#[test]
fn a_text_key_stays_text_even_when_it_looks_numeric() {
    let raw = encode(
        "name",
        "asc",
        DIGEST,
        CursorKey::Text("12345".to_string()),
        "/p",
    );
    let back = decode(&raw, "name", "asc", DIGEST).expect("round trip");
    assert_eq!(back.key, CursorKey::Text("12345".to_string()));
}

#[test]
fn a_cursor_is_opaque_hex_carrying_none_of_its_payload_in_the_clear() {
    let raw = int_cursor();
    assert!(raw.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!raw.contains("run-a"));
}

#[test]
fn non_hex_is_rejected() {
    assert_eq!(
        decode("not a cursor!", "started_at", "desc", DIGEST),
        Err(CursorError::NotHex)
    );
}

#[test]
fn hex_that_is_not_the_payload_is_rejected() {
    let raw = hex::encode(b"{\"something\":\"else\"}");
    assert_eq!(
        decode(&raw, "started_at", "desc", DIGEST),
        Err(CursorError::NotJson)
    );
}

#[test]
fn a_payload_from_another_version_is_rejected() {
    let mut cursor: Cursor = serde_json::from_slice(&hex::decode(int_cursor()).unwrap()).unwrap();
    cursor.v = 99;
    let raw = hex::encode(serde_json::to_vec(&cursor).unwrap());
    assert_eq!(
        decode(&raw, "started_at", "desc", DIGEST),
        Err(CursorError::UnknownVersion(99))
    );
}

/// Changing the walk mid-flight cannot produce a meaningful continuation, so
/// each of these is a 400 rather than a page of quietly wrong results.
#[test]
fn a_cursor_presented_against_a_different_walk_is_rejected() {
    let raw = int_cursor();
    assert_eq!(
        decode(&raw, "updated_at", "desc", DIGEST),
        Err(CursorError::SortMismatch {
            minted: "started_at".to_string(),
            requested: "updated_at".to_string(),
        })
    );
    assert_eq!(
        decode(&raw, "started_at", "asc", DIGEST),
        Err(CursorError::OrderMismatch {
            minted: "desc".to_string(),
            requested: "asc".to_string(),
        })
    );
    assert_eq!(
        decode(&raw, "started_at", "desc", "99999999"),
        Err(CursorError::FilterMismatch)
    );
}

#[test]
fn every_error_explains_itself_without_repeating_the_others() {
    let messages = [
        CursorError::NotHex.message(),
        CursorError::NotJson.message(),
        CursorError::UnknownVersion(7).message(),
        CursorError::SortMismatch {
            minted: "a".to_string(),
            requested: "b".to_string(),
        }
        .message(),
        CursorError::OrderMismatch {
            minted: "desc".to_string(),
            requested: "asc".to_string(),
        }
        .message(),
        CursorError::FilterMismatch.message(),
    ];
    for message in &messages {
        assert!(!message.is_empty());
    }
    assert!(messages[2].contains('7'));
    assert!(messages[3].contains("sort=a"));
    assert!(messages[4].contains("order=desc"));
}

#[test]
fn the_filter_digest_is_stable_and_separates_its_parts() {
    assert_eq!(
        filter_digest(&["running", "q"]),
        filter_digest(&["running", "q"])
    );
    assert_ne!(filter_digest(&["running"]), filter_digest(&["error"]));
    // Without a separator these two would hash identically.
    assert_ne!(filter_digest(&["ab", "c"]), filter_digest(&["a", "bc"]));
    assert_eq!(filter_digest(&[]).len(), 8);
}

#[test]
fn precedes_walks_descending_past_the_cursor_position() {
    let cursor = decode(&int_cursor(), "started_at", "desc", DIGEST).unwrap();
    // Older sorts after, in descending order.
    assert!(cursor.precedes(&CursorKey::Int(1_699_999_999), "run-z", true));
    // Newer sorts before - already returned on an earlier page.
    assert!(!cursor.precedes(&CursorKey::Int(1_700_000_001), "run-a", true));
    // The cursor's own item is never re-emitted.
    assert!(!cursor.precedes(&CursorKey::Int(1_700_000_000), "run-a", true));
}

/// Two items sharing a sort value is the ordinary case, not the exotic one -
/// runs start in the same second all the time. Without the id tie-break the
/// walk would drop whichever one it resumed past.
#[test]
fn precedes_breaks_a_tie_on_the_id_in_the_primary_direction() {
    let cursor = decode(&int_cursor(), "started_at", "desc", DIGEST).unwrap();
    let same = CursorKey::Int(1_700_000_000);
    // Descending: ids after "run-a" were already emitted, before it were not.
    assert!(!cursor.precedes(&same, "run-b", true));
    assert!(cursor.precedes(&same, "run-A", true));
}

#[test]
fn precedes_reverses_for_an_ascending_walk() {
    let raw = encode("started_at", "asc", DIGEST, CursorKey::Int(100), "run-m");
    let cursor = decode(&raw, "started_at", "asc", DIGEST).unwrap();
    assert!(cursor.precedes(&CursorKey::Int(101), "run-a", false));
    assert!(!cursor.precedes(&CursorKey::Int(99), "run-z", false));
    assert!(!cursor.precedes(&CursorKey::Int(100), "run-m", false));
    // Tie-break also flips.
    assert!(cursor.precedes(&CursorKey::Int(100), "run-n", false));
}
