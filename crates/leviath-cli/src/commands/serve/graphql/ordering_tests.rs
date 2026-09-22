use async_graphql::{EmptyMutation, EmptySubscription, Object, Request, Schema, SimpleObject};

use super::AnswerInSelectionOrder;

/// A leaf with two fields, so an object's own order is visible.
#[derive(SimpleObject)]
struct Pair {
    left: i32,
    right: i32,
}

struct Root;

/// Three root fields whose resolution times differ enough that the executor
/// would otherwise answer them slowest last.
#[Object]
impl Root {
    async fn slow(&self) -> Pair {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        Pair { left: 1, right: 2 }
    }

    async fn middle(&self) -> i32 {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        5
    }

    async fn fast(&self) -> Vec<Pair> {
        vec![Pair { left: 3, right: 4 }, Pair { left: 5, right: 6 }]
    }
}

fn schema() -> Schema<Root, EmptyMutation, EmptySubscription> {
    Schema::build(Root, EmptyMutation, EmptySubscription)
        .extension(AnswerInSelectionOrder)
        .finish()
}

/// The keys of the response, top level, in the order they were written.
fn keys(response: &async_graphql::Response) -> Vec<String> {
    match &response.data {
        async_graphql::Value::Object(fields) => fields.keys().map(ToString::to_string).collect(),
        other => panic!("an object, not {other:?}"),
    }
}

/// The keys of one nested object, by path from the root.
fn keys_at(response: &async_graphql::Response, path: &[&str]) -> Vec<String> {
    let mut value = &response.data;
    for step in path {
        value = match value {
            async_graphql::Value::Object(fields) => fields.get(*step).expect("the field"),
            async_graphql::Value::List(items) => items
                .get(step.parse::<usize>().expect("an index"))
                .expect("the item"),
            other => panic!("no {step} in {other:?}"),
        };
    }
    match value {
        async_graphql::Value::Object(fields) => fields.keys().map(ToString::to_string).collect(),
        other => panic!("an object, not {other:?}"),
    }
}

/// The slowest field is asked for first and answered first, and the same
/// holds inside a nested object and inside every item of a list.
#[tokio::test]
async fn fields_come_back_in_the_order_they_were_asked_for() {
    let response = schema()
        .execute(Request::new(
            "{ slow { right left } middle fast { right left } }",
        ))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(keys(&response), ["slow", "middle", "fast"]);
    assert_eq!(keys_at(&response, &["slow"]), ["right", "left"]);
    assert_eq!(keys_at(&response, &["fast", "1"]), ["right", "left"]);
}

/// A fragment spread and an inline fragment each take the place they were
/// written in, and an alias is ordered by the name the client will read.
#[tokio::test]
async fn fragments_and_aliases_keep_their_place() {
    let response = schema()
        .execute(Request::new(
            "query { ...tail second: middle ... on Root { fast { left } } slow { left } }
             fragment tail on Root { middle }",
        ))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(keys(&response), ["middle", "second", "fast", "slow"]);
}

/// A named operation is found by its name when the document holds several.
#[tokio::test]
async fn a_named_operation_is_the_one_reordered() {
    let response = schema()
        .execute(
            Request::new(
                "query A { slow { left } fast { left } }
                 query B { fast { left } middle slow { left } }",
            )
            .operation_name("B"),
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(keys(&response), ["fast", "middle", "slow"]);
}

/// A document with several operations and no name is refused before it runs,
/// and the refusal passes through with nothing to reorder.
#[tokio::test]
async fn an_unnamed_pick_among_several_operations_is_left_to_the_validator() {
    let response = schema()
        .execute(Request::new("query A { middle } query B { middle }"))
        .await;
    assert!(!response.errors.is_empty(), "the validator refuses this");
    assert_eq!(response.data, async_graphql::Value::Null);
}

/// A field mentioned twice merges into one answer in the place of the first
/// mention, which is where the executor puts it.
#[tokio::test]
async fn a_field_selected_twice_sits_where_it_was_first_mentioned() {
    let response = schema()
        .execute(Request::new("{ middle slow { left } middle }"))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(keys(&response), ["middle", "slow"]);
}

/// A query that fails to parse never reaches the reordering, and the parse
/// error is what comes back.
#[tokio::test]
async fn a_parse_failure_passes_through() {
    let response = schema().execute(Request::new("{ middle")).await;
    assert!(!response.errors.is_empty());
}

/// The spread-depth guard stops a walk that would otherwise never end. A
/// validated document cannot cycle, so the guard is exercised on the walk
/// directly.
#[test]
fn a_fragment_that_spreads_into_itself_stops_at_the_depth_guard() {
    let document =
        async_graphql::parser::parse_query("query { ...a } fragment a on Root { middle ...a }")
            .expect("parses; validation is what would refuse it");
    let operation = match &document.operations {
        async_graphql::parser::types::DocumentOperations::Single(op) => op,
        other => panic!("one operation, not {other:?}"),
    };
    let mut keys = Vec::new();
    super::response_keys(
        &operation.node.selection_set.node,
        &document.fragments,
        &mut keys,
        0,
    );
    // One key, however many times the fragment names itself: the second and
    // later mentions merge into the first, and the depth guard ends the walk.
    assert_eq!(keys.len(), 1);
}
