//! Every admin mutation answers to the admin gate.
//!
//! Read off the schema rather than listed by hand: the admin mutations are the
//! fields a server with an admin surface shows and one without hides, and each
//! of them is called with the smallest arguments its types accept. A mutation
//! added to `AdminMutation` is held to the gate without anyone adding it here.

use std::sync::Arc;

use async_graphql::Request;
use axum::http::{HeaderMap, Uri};
use serde_json::Value;

use crate::commands::serve::admin_gate::{
    ADMIN_TOKEN_HEADER, ADMIN_TOKEN_REQUIRED, AdminGate, Presented,
};
use crate::commands::serve::graphql::{LeviathSchema, build_schema};

/// A schema for a server whose admin surface is `gate`.
fn schema(gate: AdminGate) -> LeviathSchema {
    build_schema(
        crate::commands::serve::testutil::state_with_agent_paths(Vec::new()),
        gate,
    )
}

/// A server that takes admin requests only with the admin token `root`.
fn gated() -> LeviathSchema {
    schema(AdminGate::Token(Arc::new("root".to_string())))
}

/// What a request with this bearer and admin header presents.
fn presenting(bearer: &str, header: Option<&str>) -> Presented {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    if let Some(header) = header {
        headers.insert(ADMIN_TOKEN_HEADER, header.parse().unwrap());
    }
    Presented::of(&headers, &Uri::from_static("/graphql"))
}

/// Run `query` against `schema` as a request presenting `presented`.
async fn run(schema: &LeviathSchema, query: &str, presented: Presented) -> async_graphql::Response {
    schema.execute(Request::new(query).data(presented)).await
}

/// The introspection fragment that unwraps a type reference three levels deep,
/// which covers `[T!]!`.
const TYPE_REF: &str = "kind name ofType { kind name ofType { kind name ofType { kind name } } }";

/// The schema's own description of itself: every mutation field, and every
/// type with its input fields and enum values.
async fn introspect(schema: &LeviathSchema) -> Value {
    let query = format!(
        "{{ __type(name: \"Mutation\") {{ fields {{ name type {{ {TYPE_REF} }} \
         args {{ name type {{ {TYPE_REF} }} }} }} }} \
         __schema {{ types {{ name kind isOneOf enumValues {{ name }} \
         inputFields {{ name type {{ {TYPE_REF} }} }} }} }} }}"
    );
    let answer = run(schema, &query, Presented::default()).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// The names of the mutation fields `introspection` lists.
fn field_names(introspection: &Value) -> Vec<String> {
    introspection["__type"]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|field| field["name"].as_str().expect("a name").to_string())
        .collect()
}

/// The smallest literal a non-null `ty` accepts, built from the schema.
fn literal(ty: &Value, types: &[Value]) -> String {
    match ty["kind"].as_str().expect("a kind") {
        "NON_NULL" => literal(&ty["ofType"], types),
        "LIST" => "[]".to_string(),
        "ENUM" => {
            let named = named_type(ty, types);
            named["enumValues"][0]["name"]
                .as_str()
                .expect("an enum value")
                .to_string()
        }
        "INPUT_OBJECT" => {
            let named = named_type(ty, types);
            // A one-of input takes exactly one of its fields, all nullable;
            // any other takes every field it cannot do without.
            let one_of = named["isOneOf"] == true;
            let fields: Vec<String> = named["inputFields"]
                .as_array()
                .expect("input fields")
                .iter()
                .enumerate()
                .filter(|(at, field)| match one_of {
                    true => *at == 0,
                    false => field["type"]["kind"] == "NON_NULL",
                })
                .map(|(_, field)| field)
                .map(|field| {
                    format!(
                        "{}: {}",
                        field["name"].as_str().expect("a name"),
                        literal(&field["type"], types)
                    )
                })
                .collect();
            format!("{{ {} }}", fields.join(", "))
        }
        _ => match ty["name"].as_str().expect("a scalar name") {
            "Int" | "Float" => "0".to_string(),
            "Boolean" => "false".to_string(),
            _ => "\"x\"".to_string(),
        },
    }
}

/// The full type a reference names.
fn named_type<'a>(ty: &Value, types: &'a [Value]) -> &'a Value {
    let name = ty["name"].as_str().expect("a named type");
    types
        .iter()
        .find(|candidate| candidate["name"] == name)
        .expect("the type is in the schema")
}

/// Whether a field's type needs a selection set.
fn is_composite(ty: &Value) -> bool {
    match ty["kind"].as_str().expect("a kind") {
        "NON_NULL" | "LIST" => is_composite(&ty["ofType"]),
        kind => matches!(kind, "OBJECT" | "INTERFACE" | "UNION"),
    }
}

/// One call of every admin mutation, as `(name, document)`, with the smallest
/// arguments its types accept.
async fn admin_calls() -> Vec<(String, String)> {
    let shown = introspect(&gated()).await;
    let hidden = field_names(&introspect(&schema(AdminGate::Off)).await);
    let types = shown["__schema"]["types"]
        .as_array()
        .expect("types")
        .clone();
    shown["__type"]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .filter(|field| !hidden.contains(&field["name"].as_str().unwrap_or_default().to_string()))
        .map(|field| {
            let name = field["name"].as_str().expect("a name").to_string();
            let args: Vec<String> = field["args"]
                .as_array()
                .expect("args")
                .iter()
                .filter(|arg| arg["type"]["kind"] == "NON_NULL")
                .map(|arg| {
                    format!(
                        "{}: {}",
                        arg["name"].as_str().expect("a name"),
                        literal(&arg["type"], &types)
                    )
                })
                .collect();
            let args = match args.is_empty() {
                true => String::new(),
                false => format!("({})", args.join(", ")),
            };
            let selection = match is_composite(&field["type"]) {
                true => " { __typename }",
                false => "",
            };
            (
                name.clone(),
                format!("mutation {{ {name}{args}{selection} }}"),
            )
        })
        .collect()
}

/// The refusal's `extensions.code` and message.
fn refusal(answer: &async_graphql::Response) -> (String, String) {
    let error = answer.errors.first().expect("a refusal");
    let code = error
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get("code"))
        .map(ToString::to_string)
        .unwrap_or_default();
    (code, error.message.clone())
}

/// Every admin mutation refuses a request that does not present the admin
/// token, on a server that has one: as the API token alone, with a wrong
/// admin header, and with nothing at all. The refusal is the guard's, not a
/// validation error, so the call itself was well formed and only the gate
/// stood in its way.
#[tokio::test]
async fn every_admin_mutation_refuses_a_request_without_the_admin_token() {
    let calls = admin_calls().await;
    let names: Vec<&str> = calls.iter().map(|(name, _)| name.as_str()).collect();
    for expected in ["updateConfig", "upsertScript", "createMcpServer"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    let schema = gated();
    for (name, document) in &calls {
        for presented in [
            presenting("api", None),
            presenting("api", Some("nope")),
            Presented::default(),
        ] {
            let answer = run(&schema, document, presented).await;
            assert_eq!(
                refusal(&answer),
                (
                    "\"FORBIDDEN\"".to_string(),
                    ADMIN_TOKEN_REQUIRED.to_string()
                ),
                "{name}: {document}"
            );
        }
    }
}

/// On a server with no admin surface, the same calls are refused for that
/// reason instead, naming both ways to open it.
#[tokio::test]
async fn every_admin_mutation_is_refused_without_an_admin_surface() {
    let schema = schema(AdminGate::Off);
    for (name, document) in admin_calls().await {
        let (code, message) = refusal(&run(&schema, &document, presenting("root", None)).await);
        assert_eq!(code, "\"FORBIDDEN\"", "{name}");
        assert!(
            message.contains("--allow-admin or an admin token"),
            "{message}"
        );
    }
}

/// `isAdminEnabled` answers for the request asking: on a gated server it is
/// true only for a request presenting the admin token, which is what lets a
/// client ask before offering a screen that would not save.
#[tokio::test]
async fn is_admin_enabled_answers_for_the_request_asking() {
    let query = "{ config { server { isAdminEnabled } } }";
    for (schema, presented, enabled) in [
        (gated(), presenting("api", None), false),
        (gated(), presenting("root", None), true),
        (gated(), presenting("api", Some("root")), true),
        (schema(AdminGate::Open), presenting("api", None), true),
        (schema(AdminGate::Off), presenting("root", None), false),
    ] {
        let answer = run(&schema, query, presented).await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let data = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(data["config"]["server"]["isAdminEnabled"], enabled);
    }
}

/// The admin token reaches the resolver, as the bearer or in the header: the
/// answer is whatever the act itself says, not the gate's refusal.
/// `createDirectory` under a parent that does not exist is the probe, because
/// it fails inside the act and writes nothing.
#[tokio::test]
async fn the_admin_token_reaches_the_resolver() {
    let schema = gated();
    let document = "mutation { createDirectory(request: { parentPath: \"/no-such-lev-976\", \
                    name: \"a/b\" }) { __typename } }";
    for presented in [presenting("root", None), presenting("api", Some("root"))] {
        let answer = run(&schema, document, presented).await;
        let (_, message) = refusal(&answer);
        assert_ne!(message, ADMIN_TOKEN_REQUIRED);
    }
}

/// Every method of `AdminMutation` carries the guard, and is hidden with it.
/// The introspection-driven tests above find the mutations a server without an
/// admin surface hides; this catches one that forgot to hide, which those
/// would otherwise take for an ordinary mutation.
#[test]
fn every_admin_mutation_is_guarded_in_the_source() {
    let source = include_str!("mod.rs").replace("\r\n", "\n");
    let (_, body) = source
        .split_once("impl AdminMutation {")
        .expect("the admin mutations");
    let methods: Vec<&str> = body.split("    async fn ").skip(1).collect();
    assert!(methods.len() > 10, "{}", methods.len());
    let chunks: Vec<&str> = body.split("    async fn ").collect();
    let unguarded: Vec<String> = chunks
        .windows(2)
        .filter(|pair| {
            !pair[0].contains("#[graphql(visible = \"admin_visible\", guard = \"AdminGuard\")]")
        })
        .map(|pair| pair[1].split('(').next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(unguarded, Vec::<String>::new());
}
