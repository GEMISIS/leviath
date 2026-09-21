//! Tests for the schema as a whole: what it exposes, and what it refuses
//! before it runs anything.

use async_graphql::Request;

use super::{MAX_COMPLEXITY, MAX_DEPTH, build_schema, sdl};

/// A schema over a state that talks to no daemon.
fn schema() -> super::LeviathSchema {
    build_schema(
        crate::commands::serve::testutil::state_with_agent_paths(Vec::new()),
        // The published schema documents the admin surface whatever this server
        // was started with: `sdl()` ignores visibility, deliberately, so one
        // published schema describes every Leviath rather than this one.
        true,
    )
}

/// The SDL is what clients generate code from, so it carries the descriptions
/// written beside each type rather than bare field names.
#[test]
fn the_sdl_carries_the_documentation() {
    let sdl = sdl();
    assert!(sdl.contains("type Run "), "the run type is exposed");
    assert!(
        sdl.contains("Globally unique run id"),
        "field descriptions travel"
    );
    assert!(
        sdl.contains("type Run implements Node"),
        "a run is fetchable from its id alone"
    );
    assert!(
        sdl.contains("scalar Timestamp") && sdl.contains("scalar Decimal"),
        "the scalars are declared: {sdl}"
    );
    assert!(
        sdl.contains("enum RunStatus"),
        "the status vocabulary is a closed enum"
    );
}

/// Introspection works, because that is how a client discovers any of this.
#[tokio::test]
async fn the_schema_answers_introspection() {
    let answer = schema()
        .execute(Request::new("{ __schema { queryType { name } } }"))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["__schema"]["queryType"]["name"], "Query");
}

/// A query nested past the limit is refused during validation, before a single
/// file is read.
///
/// The check is what stops a client walking a sub-agent tree forever: a run's
/// children are runs, so the nesting has no natural end.
#[tokio::test]
async fn a_query_nested_too_deep_is_refused_before_it_runs() {
    // One level deeper than the limit, built from the only field that nests.
    let mut query = "id".to_string();
    for _ in 0..=MAX_DEPTH {
        query = format!("edges {{ node {{ {query} }} }}");
    }
    let answer = schema()
        .execute(Request::new(format!("{{ runs {{ {query} }} }}")))
        .await;
    let message = &answer.errors.first().expect("a refusal").message;
    assert_eq!(message, "Query is nested too deep.");
    assert!(
        answer.data.to_string() == "null",
        "nothing ran: {:?}",
        answer.data
    );
}

/// The published schema is the served one.
///
/// `docs/schema/leviath.graphql` is what clients generate code from and what
/// the docs site publishes, exactly as `openapi.json` is for the REST routes.
/// Regenerate it with `lev serve --print-graphql-schema` when this fails.
#[test]
fn the_published_schema_is_the_one_this_build_serves() {
    let published = include_str!("../../../../../../docs/schema/leviath.graphql");
    assert_eq!(published.replace("\r\n", "\n").trim_end(), sdl().trim_end());
}

/// The published schema documents the whole surface, admin included.
///
/// Two different questions: what this API *is*, which the published file
/// answers, and what a given server will *do*, which introspection answers on
/// that server. A published file that hid the admin mutations would leave a
/// client generating code against a contract it cannot see.
#[test]
fn the_published_schema_documents_the_admin_surface() {
    let sdl = sdl();
    assert!(sdl.contains("addMcpServer"), "{sdl}");
    assert!(sdl.contains("putMimeRow"), "{sdl}");
}

/// The limits are the numbers the module documents, so a change to either is a
/// deliberate edit rather than a drift.
#[test]
fn the_query_limits_are_the_documented_ones() {
    assert_eq!(MAX_DEPTH, 12);
    assert_eq!(MAX_COMPLEXITY, 10_000);
}

/// Every named type in the published schema says what it is.
///
/// A client generating code from the SDL sees the description and nothing
/// else, so a type without one is a name and a shape with no explanation
/// anywhere. The check parses the file rather than searching it, because a
/// description belongs to a definition and a search cannot tell one from a
/// sentence that happens to sit above it.
///
/// The file, not `sdl()`, because the file is what a client reads. The two
/// cannot drift: `the_published_schema_is_the_one_this_build_serves` holds
/// them byte for byte.
#[test]
fn every_named_type_says_what_it_is() {
    use async_graphql::parser::parse_schema;
    use async_graphql::parser::types::TypeSystemDefinition;

    let parsed = parse_schema(include_str!(
        "../../../../../../docs/schema/leviath.graphql"
    ))
    .expect("the schema parses");
    let mut silent: Vec<String> = parsed
        .definitions
        .iter()
        .filter_map(|definition| match definition {
            TypeSystemDefinition::Type(ty) if ty.node.description.is_none() => {
                Some(ty.node.name.node.to_string())
            }
            _ => None,
        })
        .collect();
    silent.sort();
    assert!(
        silent.is_empty(),
        "{} types carry no description: {}",
        silent.len(),
        silent.join(", ")
    );
}
