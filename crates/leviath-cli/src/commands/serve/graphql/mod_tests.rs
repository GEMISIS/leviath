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
    assert!(sdl.contains("type RunOutput "), "the run type is exposed");
    assert!(
        sdl.contains("Globally unique run id"),
        "field descriptions travel"
    );
    assert!(
        sdl.contains("type RunOutput implements Node"),
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
    for _ in 0..=MAX_DEPTH / 2 {
        query = format!("children {{ results {{ {query} }} }}");
    }
    let query = format!("results {{ {query} }}");
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
    assert!(sdl.contains("createMcpServer"), "{sdl}");
    assert!(sdl.contains("upsertMimeRow"), "{sdl}");
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

/// Every filter mirror mirrors a type that exists, field for field.
///
/// A mirror is an input object carrying all four combinators, which is what
/// `#[mirror]` writes and what nothing else in the schema has. For each one
/// there has to be an `XOutput` object, an interface `X` or a union `X`, and
/// every field it offers besides the combinators has to be a field of that
/// type. A mirror that drifts from what it mirrors is a filter a client can
/// write and the server cannot answer.
///
/// An interface is a mirror target because what every member of it promises is
/// exactly what a listing over the members may be filtered on.
#[test]
fn every_mirror_mirrors_a_type_that_exists() {
    use async_graphql::parser::parse_schema;
    use async_graphql::parser::types::{TypeKind, TypeSystemDefinition};
    use std::collections::{HashMap, HashSet};

    let parsed = parse_schema(include_str!(
        "../../../../../../docs/schema/leviath.graphql"
    ))
    .expect("the schema parses");

    let mut inputs: HashMap<String, Vec<String>> = HashMap::new();
    let mut objects: HashMap<String, HashSet<String>> = HashMap::new();
    let mut interfaces: HashMap<String, HashSet<String>> = HashMap::new();
    let mut unions: HashSet<String> = HashSet::new();
    for definition in &parsed.definitions {
        let TypeSystemDefinition::Type(ty) = definition else {
            continue;
        };
        let name = ty.node.name.node.to_string();
        match &ty.node.kind {
            TypeKind::InputObject(input) => {
                inputs.insert(
                    name,
                    input
                        .fields
                        .iter()
                        .map(|field| field.node.name.node.to_string())
                        .collect(),
                );
            }
            TypeKind::Object(object) => {
                objects.insert(
                    name,
                    object
                        .fields
                        .iter()
                        .map(|field| field.node.name.node.to_string())
                        .collect(),
                );
            }
            TypeKind::Interface(interface) => {
                interfaces.insert(
                    name,
                    interface
                        .fields
                        .iter()
                        .map(|field| field.node.name.node.to_string())
                        .collect(),
                );
            }
            TypeKind::Union(_) => {
                unions.insert(name);
            }
            _ => {}
        }
    }

    let combinators: HashSet<&str> = ["and", "or", "not", "isNull"].into_iter().collect();
    let mut wrong: Vec<String> = Vec::new();
    let mut mirrors = 0usize;
    for (name, fields) in &inputs {
        let is_mirror = combinators
            .iter()
            .all(|each| fields.iter().any(|field| field == each));
        if !is_mirror {
            continue;
        }
        mirrors += 1;
        let Some(stem) = name.strip_suffix("Input") else {
            wrong.push(format!(
                "{name} carries the combinators but does not end in `Input`"
            ));
            continue;
        };
        let output = format!("{stem}Output");
        // An interface is named as it is, because the members are the objects
        // that carry the `Output` suffix.
        let (target, mirrored) = match (objects.get(&output), interfaces.get(stem)) {
            (Some(fields), _) => (output.clone(), fields),
            (None, Some(fields)) => (stem.to_string(), fields),
            // A union's members are types of their own, so its mirror's fields
            // are variant names and there is nothing to be a subset of.
            (None, None) if unions.contains(stem) => continue,
            (None, None) => {
                wrong.push(format!(
                    "{name} mirrors no object `{output}`, interface `{stem}` or union `{stem}`"
                ));
                continue;
            }
        };
        for field in fields {
            if !combinators.contains(field.as_str()) && !mirrored.contains(field) {
                wrong.push(format!("{name}.{field} is not a field of {target}"));
            }
        }
    }
    wrong.sort();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    assert!(mirrors > 0, "the schema has mirrors to check");
}

/// The published schema, parsed once per guard below.
///
/// The file rather than `sdl()`, for the reason
/// [`every_named_type_says_what_it_is`] gives: it is what a client reads, and
/// the two cannot drift.
fn published() -> async_graphql::parser::types::ServiceDocument {
    async_graphql::parser::parse_schema(include_str!(
        "../../../../../../docs/schema/leviath.graphql"
    ))
    .expect("the schema parses")
}

/// Every type in the schema, by name, with what it is.
fn declared(
    document: &async_graphql::parser::types::ServiceDocument,
) -> Vec<(String, &async_graphql::parser::types::TypeKind)> {
    use async_graphql::parser::types::TypeSystemDefinition;

    document
        .definitions
        .iter()
        .filter_map(|definition| match definition {
            TypeSystemDefinition::Type(ty) => Some((ty.node.name.node.to_string(), &ty.node.kind)),
            _ => None,
        })
        .collect()
}

/// The name a type reference is ultimately about, with the lists and the bangs
/// taken off.
fn named(ty: &async_graphql::parser::types::Type) -> String {
    use async_graphql::parser::types::BaseType;

    let mut base = &ty.base;
    loop {
        match base {
            BaseType::Named(name) => return name.to_string(),
            BaseType::List(inner) => base = &inner.base,
        }
    }
}

/// Whether a type reference is a list of something.
fn is_list(ty: &async_graphql::parser::types::Type) -> bool {
    matches!(ty.base, async_graphql::parser::types::BaseType::List(_))
}

/// Every type name says what it is, from its suffix alone.
///
/// The whole point of the naming system: an agent reading the SDL cold should
/// know what a type is for before reading a single field. An object is
/// something read (`Output`), a page of something read (`Connection`), what an
/// act answered with (`Result`) or a live frame (`Event`); an input is a
/// filter, an order, a reference, a request, a nested write or an argument
/// bag. The three roots are named by the specification and are the only
/// exceptions.
#[test]
fn every_type_name_says_what_kind_of_type_it_is() {
    use async_graphql::parser::types::TypeKind;

    /// The three names the GraphQL specification fixes.
    const ROOTS: [&str; 3] = ["Query", "Mutation", "Subscription"];
    const OBJECT_SUFFIXES: [&str; 4] = ["Output", "Connection", "Result", "Event"];
    const INPUT_SUFFIXES: [&str; 8] = [
        "Input",
        "ListInput",
        "Filter",
        "Order",
        "Ref",
        "Request",
        "Write",
        "Options",
    ];

    let document = published();
    let mut wrong: Vec<String> = Vec::new();
    for (name, kind) in declared(&document) {
        match kind {
            TypeKind::Object(_)
                if !ROOTS.contains(&name.as_str())
                    && !OBJECT_SUFFIXES.iter().any(|end| name.ends_with(end)) =>
            {
                wrong.push(format!(
                    "object `{name}` ends in none of {}",
                    OBJECT_SUFFIXES.join(", ")
                ));
            }
            TypeKind::InputObject(_) if !INPUT_SUFFIXES.iter().any(|end| name.ends_with(end)) => {
                wrong.push(format!(
                    "input `{name}` ends in none of {}",
                    INPUT_SUFFIXES.join(", ")
                ));
            }
            _ => {}
        }
    }
    wrong.sort();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Every mutation is one act, named by one request, answering with one result.
///
/// One shape for every write, so a client that can call one can call all of
/// them: the arguments are a single `request` it can build and keep, and the
/// answer is a named object rather than a bare value. A boolean answer is the
/// shape that cannot grow: "did it work" is already in the errors, and
/// anything a caller learns later has nowhere to go.
#[test]
fn every_mutation_takes_one_request_and_answers_with_a_result() {
    use async_graphql::parser::types::TypeKind;

    /// The one mutation with nothing to say: the checks are the checks.
    const NO_REQUEST: [&str; 1] = ["checkMachine"];

    let document = published();
    let fields = declared(&document)
        .into_iter()
        .find_map(|(name, kind)| match (name.as_str(), kind) {
            ("Mutation", TypeKind::Object(object)) => Some(&object.fields),
            _ => None,
        })
        .expect("the schema has a mutation root");
    let mut wrong: Vec<String> = Vec::new();
    for field in fields {
        let name = field.node.name.node.to_string();
        let arguments: Vec<String> = field
            .node
            .arguments
            .iter()
            .map(|argument| argument.node.name.node.to_string())
            .collect();
        let wanted: &[String] = match NO_REQUEST.contains(&name.as_str()) {
            true => &[],
            false => &[String::from("request")],
        };
        if arguments != wanted {
            wrong.push(format!(
                "`{name}` takes {arguments:?} rather than {wanted:?}"
            ));
        }
        let answer = named(&field.node.ty.node);
        if !answer.ends_with("Result") {
            wrong.push(format!("`{name}` answers with `{answer}`, not a result"));
        }
    }
    wrong.sort();
    assert!(!fields.is_empty(), "the schema has mutations to check");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// An id travels as an `ID`, wherever it travels.
///
/// `ID` is what tells a client the value is a key rather than prose: it may be
/// compared, cached and handed back, and it must not be parsed or shown. A
/// field named for an id and typed `String` says the opposite of what it is.
#[test]
fn every_id_travels_as_an_id() {
    use async_graphql::parser::types::TypeKind;

    /// The names that end in `Id` and are not this schema's identities.
    ///
    /// `callId` is a provider's own correlation value, echoed back so a client
    /// can line an answer up against what the model sent; nothing here is
    /// filed under it. `modelId` is the name a provider knows a model by,
    /// which two providers can share - the identity is `ModelOutput.id`, which
    /// carries the provider as well.
    const NOT_IDENTITIES: [&str; 2] = ["callId", "modelId"];

    /// One field, as this guard reads it: its name, the type it answers with,
    /// and the name and type of each argument it takes.
    type Field = (String, String, Vec<(String, String)>);

    let document = published();
    let mut wrong: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for (owner, kind) in declared(&document) {
        let fields: Vec<Field> = match kind {
            TypeKind::Object(object) => object
                .fields
                .iter()
                .map(|field| {
                    (
                        field.node.name.node.to_string(),
                        named(&field.node.ty.node),
                        field
                            .node
                            .arguments
                            .iter()
                            .map(|argument| {
                                (
                                    argument.node.name.node.to_string(),
                                    named(&argument.node.ty.node),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
            TypeKind::Interface(interface) => interface
                .fields
                .iter()
                .map(|field| {
                    (
                        field.node.name.node.to_string(),
                        named(&field.node.ty.node),
                        Vec::new(),
                    )
                })
                .collect(),
            TypeKind::InputObject(input) => input
                .fields
                .iter()
                .map(|field| {
                    (
                        field.node.name.node.to_string(),
                        named(&field.node.ty.node),
                        Vec::new(),
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        for (field, ty, arguments) in fields {
            checked += 1;
            let is_id = |name: &str| name == "id" || name.ends_with("Id");
            if is_id(&field) && ty == "String" && !NOT_IDENTITIES.contains(&field.as_str()) {
                wrong.push(format!("{owner}.{field} is a `String`"));
            }
            for (argument, ty) in arguments {
                if is_id(&argument)
                    && ty == "String"
                    && !NOT_IDENTITIES.contains(&argument.as_str())
                {
                    wrong.push(format!("{owner}.{field}({argument}:) is a `String`"));
                }
            }
        }
    }
    wrong.sort();
    assert!(checked > 0, "the schema has fields to check");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Every frame type a subscription can ask for names a frame it can be sent.
///
/// The enum is how a client narrows a stream and the union is what it then
/// receives, so a value in one and no member in the other is a subscription
/// that asks for nothing or a frame nobody can filter to. The two are written
/// in different files, which is exactly why this is checked here.
#[test]
fn every_frame_type_names_a_frame_the_union_carries() {
    use async_graphql::parser::types::TypeKind;

    /// The name a frame type reads as: the member's name, less `Event`, in the
    /// spelling an enum value takes.
    fn screaming(member: &str) -> String {
        let stem = member.strip_suffix("Event").unwrap_or(member);
        let mut out = String::new();
        for (at, letter) in stem.chars().enumerate() {
            if letter.is_uppercase() && at > 0 {
                out.push('_');
            }
            out.extend(letter.to_uppercase());
        }
        out
    }

    let document = published();
    let types = declared(&document);
    let mut wrong: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for (enumeration, union) in [
        ("RunEventType", "RunEventFrame"),
        ("MachineEventType", "MachineEventFrame"),
    ] {
        let values: Vec<String> = types
            .iter()
            .find_map(|(name, kind)| match (name.as_str(), kind) {
                (found, TypeKind::Enum(declared)) if found == enumeration => Some(
                    declared
                        .values
                        .iter()
                        .map(|value| value.node.value.node.to_string())
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_else(Vec::new);
        let members: Vec<String> = types
            .iter()
            .find_map(|(name, kind)| match (name.as_str(), kind) {
                (found, TypeKind::Union(declared)) if found == union => Some(
                    declared
                        .members
                        .iter()
                        .map(|member| screaming(member.node.as_str()))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_else(Vec::new);
        assert!(!values.is_empty(), "{enumeration} has values");
        assert!(!members.is_empty(), "{union} has members");
        for value in &values {
            checked += 1;
            if !members.contains(value) {
                wrong.push(format!("{enumeration}.{value} names no member of {union}"));
            }
        }
    }
    wrong.sort();
    assert!(checked > 0, "the schema has frame types to check");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A listing is a connection, wherever a client can reach one.
///
/// A bare list is a promise to serve all of it, whatever it grows to. That is
/// fine for something the document it comes from bounds - a blueprint's own
/// stages, a run's metadata, the steps of one update - and wrong for anything
/// that accumulates. The allowlist is therefore the bounded lists, written out
/// so that adding a new one is a decision somebody made rather than a default.
#[test]
fn every_listing_a_client_can_reach_is_a_connection() {
    use async_graphql::parser::types::TypeKind;

    /// The bounded lists: each one is as long as the document or the act it
    /// belongs to, and none of them grows with use.
    const BOUNDED: [&str; 10] = [
        // One answer per id asked about, so the client sets the length.
        "Query.nodes",
        // The group tokens a stage may name: a handful, fixed by this build.
        "Query.toolGroups",
        // A manifest's own contents, bounded by the document.
        "BlueprintOutput.stages",
        "BlueprintOutput.regions",
        "BlueprintOutput.dependencies",
        "BlueprintOutput.mimeTypes",
        "BlueprintOutput.transforms",
        // One row per stage the blueprint declares.
        "RunOutput.stageModels",
        // What the spawn attached, which the spawn wrote.
        "RunOutput.metadata",
        // The steps of one update, fixed by this build.
        "UpdateJobOutput.steps",
    ];

    let document = published();
    let types = declared(&document);
    let objects: Vec<(String, &async_graphql::parser::types::ObjectType)> = types
        .iter()
        .filter_map(|(name, kind)| match kind {
            TypeKind::Object(object) => Some((name.clone(), object)),
            _ => None,
        })
        .collect();
    let describes = |name: &str| {
        types.iter().any(|(found, kind)| {
            found == name
                && matches!(
                    kind,
                    TypeKind::Object(_) | TypeKind::Interface(_) | TypeKind::Union(_)
                )
        })
    };

    let mut wrong: Vec<String> = Vec::new();
    for (owner, object) in &objects {
        let reachable = owner == "Query"
            || object
                .implements
                .iter()
                .any(|interface| interface.node.as_str() == "Node");
        if !reachable {
            continue;
        }
        for field in &object.fields {
            let name = field.node.name.node.to_string();
            if !is_list(&field.node.ty.node) || !describes(&named(&field.node.ty.node)) {
                continue;
            }
            let at = format!("{owner}.{name}");
            if !BOUNDED.contains(&at.as_str()) {
                wrong.push(format!(
                    "{at} serves a bare list of objects; page it or name it in BOUNDED"
                ));
            }
        }
    }
    wrong.sort();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
