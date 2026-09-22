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

/// The encoding is a promise to every client holding a token across a server
/// restart, so it is pinned to the exact bytes rather than to a round trip,
/// which would keep passing if both halves moved together.
#[test]
fn an_existing_cursor_string_still_encodes_byte_for_byte() {
    let bytes = hex::decode(int_cursor()).expect("hex");
    assert_eq!(
        String::from_utf8(bytes).expect("utf8"),
        concat!(
            r#"{"v":1,"sort":"started_at","order":"desc","digest":"abcd1234","#,
            r#""key":{"Int":1700000000},"id":"run-a"}"#
        )
    );
}

#[test]
fn a_null_key_and_a_tuple_key_survive_the_trip() {
    let raw = encode("title", "asc", DIGEST, CursorKey::Null, "run-a");
    assert_eq!(
        decode(&raw, "title", "asc", DIGEST)
            .expect("round trip")
            .key,
        CursorKey::Null
    );
    let tuple = CursorKey::Tuple(vec![
        CursorKey::Int(7),
        CursorKey::Text("b".to_string()),
        CursorKey::Null,
    ]);
    let sort = "started_at:desc,title:asc";
    let raw = encode(sort, "desc", DIGEST, tuple.clone(), "run-a");
    assert_eq!(
        decode(&raw, sort, "desc", DIGEST).expect("round trip").key,
        tuple
    );
}

/// Nulls sort last going up and first going down. This is the one place the
/// cross-variant ordering meets real data: a listing ordered by a field that
/// some items simply do not have.
#[test]
fn a_missing_value_sorts_to_the_far_end() {
    let present = (&CursorKey::Text("a".to_string()), "run-a");
    let absent = (&CursorKey::Null, "run-b");
    assert_eq!(compare(present, absent, &[false]), Ordering::Less);
    assert_eq!(compare(present, absent, &[true]), Ordering::Greater);
}

#[test]
fn a_tuple_key_compares_each_component_in_its_own_direction() {
    let a_first = CursorKey::Tuple(vec![CursorKey::Int(1), CursorKey::Text("a".into())]);
    let b_second = CursorKey::Tuple(vec![CursorKey::Int(1), CursorKey::Text("b".into())]);
    let newer = CursorKey::Tuple(vec![CursorKey::Int(2), CursorKey::Text("z".into())]);
    // Newest first, then title A to Z.
    let dirs = [true, false];
    assert_eq!(
        compare((&newer, "x"), (&a_first, "x"), &dirs),
        Ordering::Less
    );
    assert_eq!(
        compare((&a_first, "x"), (&b_second, "x"), &dirs),
        Ordering::Less
    );
    // Every component equal: the id decides, in the primary direction.
    assert_eq!(
        compare((&newer, "run-b"), (&newer, "run-a"), &dirs),
        Ordering::Less
    );
    assert_eq!(
        compare((&newer, "run-a"), (&newer, "run-a"), &dirs),
        Ordering::Equal
    );
}

/// A shorter tuple meeting a longer one only happens if an order changed under
/// a cursor a client was still holding; the length decides, so the comparison
/// stays total and the walk still terminates.
#[test]
fn a_shorter_tuple_sorts_before_a_longer_one_that_starts_the_same() {
    let short = CursorKey::Tuple(vec![CursorKey::Int(1)]);
    let long = CursorKey::Tuple(vec![CursorKey::Int(1), CursorKey::Int(2)]);
    assert_eq!(
        compare((&short, "a"), (&long, "a"), &[false]),
        Ordering::Less
    );
    assert_eq!(
        compare((&short, "a"), (&long, "a"), &[true]),
        Ordering::Greater
    );
}

/// With no direction given at all the walk runs ascending, rather than taking
/// whatever an empty slice happens to yield.
#[test]
fn an_empty_direction_list_reads_as_ascending() {
    assert_eq!(
        compare((&CursorKey::Int(1), "a"), (&CursorKey::Int(2), "a"), &[]),
        Ordering::Less
    );
    assert_eq!(order_name(false), "asc");
    assert_eq!(order_name(true), "desc");
}

#[test]
fn a_per_component_walk_agrees_with_the_single_key_one() {
    let cursor = decode(&int_cursor(), "started_at", "desc", DIGEST).expect("round trip");
    assert!(cursor.precedes_in(&CursorKey::Int(1_699_999_999), "run-z", &[true]));
    assert!(!cursor.precedes_in(&CursorKey::Int(1_700_000_001), "run-a", &[true]));
}

#[test]
fn a_position_cursor_round_trips_and_refuses_another_listings_token() {
    let raw = encode_position(DIGEST, 7, false);
    assert_eq!(decode_position(&raw, DIGEST, false), Ok(7));
    // Minted under one filter set, presented under another.
    assert_eq!(
        decode_position(&raw, "ffffffff", false),
        Err(CursorError::FilterMismatch)
    );
    // The same listing, read the other way round, is a different walk.
    assert_eq!(
        decode_position(&raw, DIGEST, true),
        Err(CursorError::OrderMismatch {
            minted: "asc".to_string(),
            requested: "desc".to_string(),
        })
    );
}

#[test]
fn a_position_cursor_whose_key_is_not_a_position_is_refused() {
    let lettered = encode(
        POSITION_SORT,
        "asc",
        DIGEST,
        CursorKey::Text("7".into()),
        "",
    );
    assert_eq!(
        decode_position(&lettered, DIGEST, false),
        Err(CursorError::NotPositional)
    );
    let negative = encode(POSITION_SORT, "asc", DIGEST, CursorKey::Int(-1), "");
    assert_eq!(
        decode_position(&negative, DIGEST, false),
        Err(CursorError::NotPositional)
    );
    assert!(CursorError::NotPositional.message().contains("position"));
}
