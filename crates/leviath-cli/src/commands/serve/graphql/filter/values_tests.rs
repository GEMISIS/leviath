//! Which filter each kind of value is tested with.

use async_graphql::ID;

use super::{
    BigInt, BigIntFilter, BooleanFilter, CursorKey, Decimal, DecimalFilter, Filterable,
    FloatFilter, IDFilter, IntFilter, JSONFilter, Json, MatchCx, Sortable, StringFilter,
    StringListFilter, Timestamp, TimestampFilter, Tri, option_test,
};

/// A filter that only ever matches one string.
fn only(text: &str) -> StringFilter {
    StringFilter {
        eq: Some(text.to_owned()),
        ..StringFilter::default()
    }
}

/// Every scalar reaches the filter its own type is compared with.
#[tokio::test]
async fn every_scalar_has_its_filter() {
    let cx = MatchCx::at(0);
    assert_eq!("plan".test(&only("plan"), &cx), Tri::Yes);
    assert_eq!("plan".to_owned().test(&only("plan"), &cx), Tri::Yes);
    assert_eq!(
        ID::from("run:one").test(
            &IDFilter {
                eq: Some(ID::from("run:one")),
                ..IDFilter::default()
            },
            &cx
        ),
        Tri::Yes
    );
    assert_eq!(
        5_i32.test(
            &IntFilter {
                eq: Some(5),
                ..IntFilter::default()
            },
            &cx
        ),
        Tri::Yes
    );
    assert_eq!(
        5_i64.test(
            &BigIntFilter {
                eq: Some(BigInt(5)),
                ..BigIntFilter::default()
            },
            &cx
        ),
        Tri::Yes
    );
    assert_eq!(BigInt(5).test(&BigIntFilter::default(), &cx), Tri::Yes);
    assert_eq!(0.5_f64.test(&FloatFilter::default(), &cx), Tri::Yes);
    assert_eq!(Decimal(0.5).test(&DecimalFilter::default(), &cx), Tri::Yes);
    assert_eq!(
        Timestamp(5).test(&TimestampFilter::default(), &cx),
        Tri::Yes
    );
    assert_eq!(true.test(&BooleanFilter::default(), &cx), Tri::Yes);
    assert_eq!(
        Json(serde_json::json!(1)).test(&JSONFilter::default(), &cx),
        Tri::Yes
    );

    // Every one of them is settled, so confirming reads nothing.
    assert!("plan".confirm(&only("plan"), &cx).await);
    assert!("plan".to_owned().confirm(&only("plan"), &cx).await);
    assert!(5_i32.confirm(&IntFilter::default(), &cx).await);
    assert!(5_i64.confirm(&BigIntFilter::default(), &cx).await);
    assert!(BigInt(5).confirm(&BigIntFilter::default(), &cx).await);
    assert!(0.5_f64.confirm(&FloatFilter::default(), &cx).await);
    assert!(Decimal(0.5).confirm(&DecimalFilter::default(), &cx).await);
    assert!(Timestamp(5).confirm(&TimestampFilter::default(), &cx).await);
    assert!(true.confirm(&BooleanFilter::default(), &cx).await);
    assert!(
        Json(serde_json::json!(1))
            .confirm(&JSONFilter::default(), &cx)
            .await
    );
    assert!(ID::from("run:one").confirm(&IDFilter::default(), &cx).await);
}

/// A reference is filtered as the thing it points at.
#[tokio::test]
async fn a_reference_is_the_value_it_points_at() {
    let cx = MatchCx::at(0);
    let value = "plan";
    assert_eq!((&value).test(&only("plan"), &cx), Tri::Yes);
    assert!((&value).confirm(&only("plan"), &cx).await);
}

/// A value that is not there matches only where absence was asked for.
#[tokio::test]
async fn absence_is_answered_in_one_place() {
    let cx = MatchCx::at(0);
    let absent: Option<String> = None;
    let present = Some("plan".to_owned());

    let asking = StringFilter {
        is_null: Some(true),
        ..StringFilter::default()
    };
    assert_eq!(absent.test(&asking, &cx), Tri::Yes);
    assert_eq!(present.test(&asking, &cx), Tri::No);
    assert!(absent.confirm(&asking, &cx).await);
    assert!(!present.confirm(&asking, &cx).await);

    assert_eq!(absent.test(&only("plan"), &cx), Tri::No);
    assert_eq!(present.test(&only("plan"), &cx), Tri::Yes);
    assert!(!absent.confirm(&only("plan"), &cx).await);
    assert!(present.confirm(&only("plan"), &cx).await);

    assert_eq!(option_test(None::<&String>, &asking, &cx), Tri::Yes);
}

/// A field that could not be read never matches.
#[tokio::test]
async fn a_failed_read_never_matches() {
    let cx = MatchCx::at(0);
    let read: Result<String, &str> = Ok("plan".to_owned());
    let failed: Result<String, &str> = Err("gone");
    assert_eq!(read.test(&only("plan"), &cx), Tri::Yes);
    assert_eq!(failed.test(&only("plan"), &cx), Tri::No);
    assert!(read.confirm(&only("plan"), &cx).await);
    assert!(!failed.confirm(&only("plan"), &cx).await);
}

/// A list of strings is filtered by membership, as a slice or a `Vec`.
#[tokio::test]
async fn a_list_of_strings_is_filtered_by_membership() {
    let cx = MatchCx::at(0);
    let holds = StringListFilter {
        has: Some("one".to_owned()),
        ..StringListFilter::default()
    };
    let items = vec!["one".to_owned(), "two".to_owned()];
    assert_eq!(items.test(&holds, &cx), Tri::Yes);
    assert_eq!(items[..].test(&holds, &cx), Tri::Yes);
    assert!(items.confirm(&holds, &cx).await);
    assert!(items[..].confirm(&holds, &cx).await);

    let borrowed = vec!["one", "two"];
    assert_eq!(borrowed.test(&holds, &cx), Tri::Yes);
    assert!(borrowed.confirm(&holds, &cx).await);
}

/// Every sortable value answers with the key the cursor encodes.
#[test]
fn every_sortable_value_has_a_key() {
    assert_eq!("one".cursor_key(), CursorKey::Text("one".to_owned()));
    assert_eq!(
        "one".to_owned().cursor_key(),
        CursorKey::Text("one".to_owned())
    );
    assert_eq!(
        ID::from("run:one").cursor_key(),
        CursorKey::Text("run:one".to_owned())
    );
    assert_eq!(1_i32.cursor_key(), CursorKey::Int(1));
    assert_eq!(1_i64.cursor_key(), CursorKey::Int(1));
    assert_eq!(BigInt(1).cursor_key(), CursorKey::Int(1));
    assert_eq!(Timestamp(1).cursor_key(), CursorKey::Int(1));
    assert_eq!((&"one").cursor_key(), CursorKey::Text("one".to_owned()));
    assert_eq!(Some(1_i32).cursor_key(), CursorKey::Int(1));
    assert_eq!(None::<i32>.cursor_key(), CursorKey::Null);
}
