//! Tests for the compiled order and the cursors it mints.

use super::{Order, OrderDirection, OrderField, Orderable, Term};
use crate::commands::serve::cursor::{self, CursorKey};

/// A two-field order, standing in for the generated `RunOrderField`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    StartedAt,
    Title,
}

impl OrderField for Field {
    fn wire(self) -> &'static str {
        match self {
            Field::StartedAt => "started_at",
            Field::Title => "title",
        }
    }
}

/// The item being ordered, and the context its keys are read against.
struct Row {
    started_at: i64,
    title: Option<String>,
}

/// Stands in for the match context a real key may consult.
struct Clock;

impl Orderable<Clock> for Row {
    type Field = Field;

    fn key(&self, field: Field, _cx: &Clock) -> CursorKey {
        match field {
            Field::StartedAt => CursorKey::Int(self.started_at),
            Field::Title => match &self.title {
                Some(title) => CursorKey::Text(title.clone()),
                None => CursorKey::Null,
            },
        }
    }
}

fn term(field: Field, direction: OrderDirection) -> Term<Field> {
    Term { field, direction }
}

fn row() -> Row {
    Row {
        started_at: 1_700_000_000,
        title: Some("build".to_string()),
    }
}

#[test]
fn a_direction_says_which_way_it_runs_and_what_a_cursor_records() {
    assert!(!OrderDirection::Asc.descending());
    assert!(OrderDirection::Desc.descending());
    assert_eq!(OrderDirection::Asc.wire(), "asc");
    assert_eq!(OrderDirection::Desc.wire(), "desc");
}

/// The whole point of the single-key case: what this mints is byte for byte
/// what `GET /api/runs` mints, so a cursor crosses between the two surfaces.
#[test]
fn one_key_encodes_exactly_as_the_rest_listing_does() {
    let order = Order::new(vec![term(Field::StartedAt, OrderDirection::Desc)]);
    assert_eq!(order.sort(), "started_at");
    assert_eq!(order.order(), "desc");
    assert_eq!(order.descending(), &[true]);

    let position = order.position(&row(), "run-a".to_string(), &Clock);
    assert_eq!(position.key, CursorKey::Int(1_700_000_000));
    assert_eq!(position.id, "run-a");

    let minted = order.encode("abcd1234", &position);
    let rest = cursor::encode(
        "started_at",
        "desc",
        "abcd1234",
        CursorKey::Int(1_700_000_000),
        "run-a",
    );
    assert_eq!(minted, rest);
}

#[test]
fn two_keys_spell_out_every_direction_and_carry_a_tuple() {
    let order = Order::new(vec![
        term(Field::StartedAt, OrderDirection::Desc),
        term(Field::Title, OrderDirection::Asc),
    ]);
    assert_eq!(order.sort(), "started_at:desc,title:asc");
    // The primary direction, which is what the id tie-break follows.
    assert_eq!(order.order(), "desc");
    assert_eq!(order.descending(), &[true, false]);

    let position = order.position(&row(), "run-a".to_string(), &Clock);
    assert_eq!(
        position.key,
        CursorKey::Tuple(vec![
            CursorKey::Int(1_700_000_000),
            CursorKey::Text("build".to_string()),
        ])
    );
}

/// An item with no value for the ordering field still has a place in the walk,
/// which is what keeps the keyset comparison total.
#[test]
fn an_item_with_no_value_for_the_field_keys_as_null() {
    let order = Order::new(vec![term(Field::Title, OrderDirection::Asc)]);
    let untitled = Row {
        started_at: 1,
        title: None,
    };
    let position = order.position(&untitled, "run-a".to_string(), &Clock);
    assert_eq!(position.key, CursorKey::Null);
}

/// No terms is not a listing anybody asks for, but it has to be a total order
/// rather than a panic: the id alone does the whole job.
#[test]
fn an_empty_order_leaves_the_id_to_do_the_ordering() {
    let order: Order<Field> = Order::new(Vec::new());
    assert_eq!(order.sort(), "");
    assert_eq!(order.order(), "asc");
    assert!(order.descending().is_empty());
    let position = order.position(&row(), "run-a".to_string(), &Clock);
    assert_eq!(position.key, CursorKey::Null);
}

#[test]
fn a_cursor_round_trips_through_the_order_that_minted_it() {
    let order = Order::new(vec![
        term(Field::StartedAt, OrderDirection::Desc),
        term(Field::Title, OrderDirection::Asc),
    ]);
    let position = order.position(&row(), "run-a".to_string(), &Clock);
    let raw = order.encode("abcd1234", &position);
    let back = order.decode(&raw, "abcd1234").expect("round trip");
    assert_eq!(back.key, position.key);
    assert_eq!(back.id, "run-a");
}

/// Changing the order mid-walk cannot produce a meaningful continuation, so it
/// is a refusal the client can act on rather than a page of wrong rows.
#[test]
fn a_cursor_from_another_order_is_refused() {
    let minted_by = Order::new(vec![term(Field::StartedAt, OrderDirection::Desc)]);
    let position = minted_by.position(&row(), "run-a".to_string(), &Clock);
    let raw = minted_by.encode("abcd1234", &position);

    let other_field = Order::new(vec![term(Field::Title, OrderDirection::Desc)]);
    let refusal = other_field.decode(&raw, "abcd1234").expect_err("refused");
    assert_eq!(refusal.code(), "BAD_USER_INPUT");
    assert!(refusal.to_string().contains("sort=started_at"));

    let other_way = Order::new(vec![term(Field::StartedAt, OrderDirection::Asc)]);
    assert!(
        other_way
            .decode(&raw, "abcd1234")
            .expect_err("refused")
            .to_string()
            .contains("order=desc")
    );
}

/// The digest is the other half of the same promise: same order, different
/// filter, still a different walk.
#[test]
fn a_cursor_from_another_filter_is_refused() {
    let order = Order::new(vec![term(Field::StartedAt, OrderDirection::Desc)]);
    let position = order.position(&row(), "run-a".to_string(), &Clock);
    let raw = order.encode("abcd1234", &position);
    assert!(
        order
            .decode(&raw, "ffffffff")
            .expect_err("refused")
            .to_string()
            .contains("different set of filters")
    );
}
